//! High-level NFSv4.1 operations on top of the session.

// bindgen emits lowercase constants (e.g. nfs_opnum4_NFS4_OP_WRITE) matched
// here in patterns; silence the style lint for those.
#![allow(non_upper_case_globals)]

use std::os::raw::c_char;
#[cfg(feature = "test-faults")]
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use nfsv41_sys::*;

use crate::compound::{Compound, CompoundRes};
use crate::planner::{
    AdaptiveCompoundLimits, ExecutionMap, FailureCause, RecoveryAction, RequestSafety,
    recovery_action, resource_rejected_before_mutation,
};
use crate::session::Session;
use vfsi_core::path::split_path_bytes;
use vfsi_core::{RpcError, RpcResult};
mod completion;
mod io_progress;
mod open;
mod path_mutation;
use io_progress::{complete_read_suffix, complete_write_suffix};
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};

/// An uncached duplicate reply hides the entire original compound, even
/// though its SEQUENCE succeeded. Keep the native NFS cause independently of
/// certainty instead of pretending this protocol status is a transport error.
pub(crate) fn uncertain_completion(cause: &vfsi_core::VfError) -> bool {
    cause.is_transport()
        || cause.status()
            == Some(vfsi_core::StatusCode::Nfs(
                nfsstat4_NFS4ERR_RETRY_UNCACHED_REP,
            ))
}

/// Opaque NFS protocol reference to a server-side filesystem object.
///
/// This addresses the object for NFS operations; it is not the
/// stateful open instance, which also involves NFS OPEN state such as a
/// stateid.
///
/// Owns a fixed-length byte slice. Cloning copies the opaque bytes; it does
/// not create or duplicate NFS OPEN state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NfsFh {
    bytes: Box<[u8]>,
}

/// Which open owner a client operation uses: the user-visible descriptor
/// owner, or the internal path-op owner whose stateids never collide with
/// caller-held descriptors.
#[derive(Clone, Copy)]
enum OwnerSlot {
    User,
    Path,
}

impl NfsFh {
    #[cfg(test)]
    pub(crate) fn test_from_bytes(bytes: impl Into<Box<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn as_nfs_fh(&self) -> nfs_fh4 {
        nfs_fh4 {
            nfs_fh4_len: self.bytes.len() as u32,
            nfs_fh4_val: self.bytes.as_ptr() as *mut c_char,
        }
    }

    fn from_nfs_fh(fh: &nfs_fh4) -> NfsFh {
        let len = fh.nfs_fh4_len as usize;
        let bytes: Box<[u8]> = if len == 0 {
            // A zero-length XDR opaque value may have a null pointer, while
            // from_raw_parts requires a non-null pointer even for length 0.
            Vec::new().into_boxed_slice()
        } else {
            // SAFETY: fh is borrowed from a live decoded XDR reply; its nonzero length
            // describes initialized bytes, copied before that reply is freed.
            let slice = unsafe { std::slice::from_raw_parts(fh.nfs_fh4_val as *const u8, len) };
            Box::from(slice)
        };
        NfsFh { bytes }
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
    root: NfsFh,
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
    // An OPEN may exist without an exact filehandle. Never renew its identity
    // indefinitely or try to recover cleanup through a diagnostic pathname.
    quarantined_opens: bool,
    next_public_open_owner_id: u64,
    #[cfg(feature = "test-faults")]
    fault_injector: Option<Arc<dyn FaultInjector>>,
    #[cfg(feature = "test-faults")]
    confirmed_path_closes: usize,
    #[cfg(feature = "test-faults")]
    reject_next_compound_tag: Option<Box<[u8]>>,
    #[cfg(feature = "test-faults")]
    short_merged_read_once: Option<usize>,
    #[cfg(feature = "test-faults")]
    short_merged_write_once: Option<usize>,
    #[cfg(feature = "test-faults")]
    fail_merged_write_once: Option<(usize, usize)>,
    #[cfg(feature = "test-faults")]
    pub(crate) fail_trailing_write_close_once: bool,
    #[cfg(feature = "test-faults")]
    pub(crate) fail_inline_path_close_once: bool,
    #[cfg(feature = "test-faults")]
    pub(crate) fail_merged_read_once: Option<usize>,
    #[cfg(feature = "test-faults")]
    pub(crate) fail_merged_truncate_once: bool,
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

/// A READDIR position and the verifier returned with it. Start with Default;
/// pass returned positions unchanged. A returned value of zero denotes EOF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirCookie {
    pub value: u64,
    pub verifier: verifier4,
}

/// One visible page and its continuation position (zero at EOF).
pub type ReaddirPage = (Vec<DirEntry>, DirCookie);

/// An entry returned by READDIR.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub cookie: u64,
    /// The fields actually present in attrs, in increasing attribute-ID order.
    pub attrmask: bitmap4,
    pub attrs: Vec<u8>,
}

/// A directory's listing (entries so far and the cookie to continue).
pub struct ChildListing {
    pub fh: NfsFh,
    pub entries: Vec<DirEntry>,
    pub cookie: DirCookie,
}

/// One READ of a batched compound, `[PUTFH, READ]`.
pub struct ReadOp {
    pub fh: NfsFh,
    pub stateid: stateid4,
    pub offset: u64,
    pub count: u32,
}

/// One WRITE of a batched compound, `[PUTFH, WRITE]`.
pub struct WriteOp<'a> {
    pub fh: NfsFh,
    pub stateid: stateid4,
    pub offset: u64,
    pub data: &'a [u8],
}

/// Returned field bitmap and its XDR-encoded values.
pub(crate) type ReturnedAttrs = (bitmap4, Vec<u8>);

/// One GETATTR of a batched compound, `[PUTFH, GETATTR]`.
pub struct GetattrOp {
    pub fh: NfsFh,
    pub attrs: Vec<u32>,
}

/// One SETATTR of a batched compound, `[PUTFH, SETATTR]`.
pub struct SetattrOp {
    pub fh: NfsFh,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<(i64, u32)>,
    pub mtime: Option<(i64, u32)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct VerifierStorageAttrs {
    mode: bool,
    atime: bool,
    mtime: bool,
}

fn verifier_storage_attrs(attrset: &bitmap4) -> VerifierStorageAttrs {
    let has = |attribute: u32| {
        let word = (attribute / 32) as usize;
        word < attrset.bitmap4_len as usize
            && word < attrset.map.len()
            && attrset.map[word] & (1 << (attribute % 32)) != 0
    };
    VerifierStorageAttrs {
        mode: has(FATTR4_MODE),
        atime: has(FATTR4_TIME_ACCESS) || has(FATTR4_TIME_ACCESS_SET),
        mtime: has(FATTR4_TIME_MODIFY) || has(FATTR4_TIME_MODIFY_SET),
    }
}

fn current_timestamp() -> (i64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (
        now.as_secs().min(i64::MAX as u64) as i64,
        now.subsec_nanos(),
    )
}

fn verifier_cleanup_setattr(
    fh: &NfsFh,
    attrs: VerifierStorageAttrs,
    mode: Option<u32>,
    timestamp: (i64, u32),
) -> SetattrOp {
    SetattrOp {
        fh: fh.clone(),
        mode: mode.or(attrs.mode.then_some(0o600)),
        uid: None,
        gid: None,
        size: None,
        atime: attrs.atime.then_some(timestamp),
        mtime: attrs.mtime.then_some(timestamp),
    }
}

/// One READLINK of a batched compound, `[PUTFH, READLINK]`.
pub struct ReadlinkOp {
    pub fh: NfsFh,
}

/// One RENAME of a batched compound: `[PUTFH src, SAVEFH, PUTFH dst, RENAME]`.
pub struct RenameOp {
    pub srcdir: NfsFh,
    pub oldname: Vec<u8>,
    pub dstdir: NfsFh,
    pub newname: Vec<u8>,
}

/// One CREATE of a batched compound, `[PUTFH dir, CREATE]` (mkdir / symlink).
pub struct CreateOp {
    pub dir: NfsFh,
    pub name: Vec<u8>,
    pub ftype: nfs_ftype4,
    pub linkdata: Option<Vec<u8>>,
    /// Optional permissions supplied atomically with creation.
    pub mode: Option<u32>,
}

/// One LINK of a batched compound: `[PUTFH src, SAVEFH, PUTFH dst, LINK]`.
pub struct LinkOp {
    pub dstdir: NfsFh,
    pub src: NfsFh,
    pub newname: Vec<u8>,
}

/// One OPEN of a batched compound, `[PUTFH dir, OPEN, GETFH]`.
pub struct OpenOp {
    pub dir: NfsFh,
    pub name: Vec<u8>,
    pub access: u32,
    pub create: OpenCreate,
}

/// One CLOSE of a batched compound, `[PUTFH fh, CLOSE]`.
pub struct CloseOp {
    pub fh: NfsFh,
    pub stateid: stateid4,
}

/// Close path-batch OPENs if a later preparation step fails, while preserving
/// the original failure if best-effort cleanup also fails.
#[cfg(test)]
pub(crate) fn prepare_path_opens<C, T, E>(
    opened: &[(NfsFh, stateid4)],
    client: &mut C,
    prepare: impl FnOnce(&mut C) -> Result<T, E>,
    close: impl FnOnce(&mut C, &[CloseOp]),
) -> Result<T, E> {
    let result = prepare(client);
    if result.is_err() && !opened.is_empty() {
        let closes: Vec<_> = opened
            .iter()
            .map(|(fh, stateid)| CloseOp {
                fh: fh.clone(),
                stateid: *stateid,
            })
            .collect();
        close(client, &closes);
    }
    result
}

/// One NFSv4.2 COPY: `[PUTFH src, SAVEFH, PUTFH dst, COPY]`.
pub struct CopyOp {
    pub src_fh: NfsFh,
    pub src_stateid: stateid4,
    pub dst_fh: NfsFh,
    pub dst_stateid: stateid4,
    pub src_offset: u64,
    pub dst_offset: u64,
    /// Zero means copy from `src_offset` through EOF.
    pub count: u64,
}

fn validate_copy_result(op: &CopyOp, reply: &COPY4resok) -> vfsi_core::OpResult<u64> {
    let count = reply.cr_response.wr_count;
    if reply.cr_response.wr_ids != 0
        || reply.cr_synchronous != 1
        || reply.cr_consecutive != 1
        || reply.cr_response.wr_committed > stable_how4_FILE_SYNC4
        || op.count != 0 && count > op.count
        || op.src_offset.checked_add(count).is_none()
        || op.dst_offset.checked_add(count).is_none()
    {
        return Err(vfsi_core::OpError::unknown(
            vfsi_core::VfError::invalid_reply(None, "invalid synchronous COPY response"),
            None,
        ));
    }
    Ok(count)
}

fn readdir_response_limits(
    response_bytes: usize,
    operations: usize,
    page_bytes: usize,
) -> (u32, u32) {
    let maxcount = (response_bytes / operations.max(1))
        .max(1)
        .min(u32::MAX as usize)
        .min(page_bytes.max(4096)) as u32;
    ((maxcount / 4).max(1), maxcount)
}

/// The server confirmed `ca_maxoperations` from CREATE_SESSION; keep every
/// compound (plus the implicit SEQUENCE) under it.
const MAX_COMPOUND_OPS: usize = 256;

#[cfg(test)]
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
pub const READDIR_ATTRS: [u32; 15] = [
    FATTR4_TYPE,
    FATTR4_CHANGE,
    FATTR4_SIZE,
    FATTR4_NAMED_ATTR,
    FATTR4_FSID,
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
#[derive(PartialEq, Eq, Hash)]
pub enum FileRef {
    Path(Vec<u8>),
    Handle(NfsFh),
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
    opened_path: &'a mut Option<Box<[u8]>>,
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
    dispatched: &'a mut bool,
    submitted: &'a mut Vec<(usize, usize, usize)>,
}

/// Same-object dependencies for one NFS WRITE wave. The first range
/// needs no tree allocation; additional independent ranges are indexed in
/// O(log n), avoiding quadratic overlap checks for large scatter vectors.
pub(crate) struct WriteWaveAccess {
    positional: bool,
    first: std::ops::Range<u64>,
    additional: std::collections::BTreeMap<u64, u64>,
}

impl WriteWaveAccess {
    pub(crate) fn new(range: std::ops::Range<u64>, positional: bool) -> Self {
        Self {
            positional,
            first: range,
            additional: std::collections::BTreeMap::new(),
        }
    }

    pub(crate) fn admit(&mut self, range: std::ops::Range<u64>, positional: bool) -> bool {
        if !self.positional || !positional {
            return false;
        }
        // Empty writes do not overlap, and must not replace a nonempty
        // interval with the same start in the overlap index.
        if range.is_empty() {
            return true;
        }
        if !self.first.is_empty() && range.start < self.first.end && self.first.start < range.end {
            return false;
        }
        // Already admitted intervals are disjoint: only the predecessor of
        // this range's end can overlap it, even for nonmonotonic offsets.
        if self
            .additional
            .range(..range.end)
            .next_back()
            .is_some_and(|(_, &end)| end > range.start)
        {
            return false;
        }
        self.additional.insert(range.start, range.end);
        true
    }
}

/// Independent merged WRITE results and temporary-open cleanup ownership.
pub struct PathWriteOutcome {
    pub results: Vec<vfsi_core::OpResult<(usize, u32)>>,
    pub opened: Vec<(NfsFh, stateid4)>,
    /// The special CLOSE failed or did not fit; subsequent calls can use
    /// separate closes. This hint never asks a caller to replay WRITEs.
    pub close_failed: Option<u32>,
}

/// Whether an NFS status indicates the special stateid was rejected
/// (rather than a per-file failure), so the merged path must be disabled.
pub(crate) fn is_stateid_error(status: u32) -> bool {
    matches!(
        status,
        nfsstat4_NFS4ERR_BAD_STATEID
            | nfsstat4_NFS4ERR_OLD_STATEID
            | nfsstat4_NFS4ERR_STALE_STATEID
            | nfsstat4_NFS4ERR_BAD_SEQID
            | nfsstat4_NFS4ERR_NOTSUPP
    )
}

/// A protocol-proven path rejection that the native namespace layer can repair
/// before any later logical mutation. This is private continuation metadata,
/// never a completed-prefix guarantee in the public vector API.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathWriteRepair {
    pub index: usize,
    pub kind: PathWriteRepairKind,
    pub prefix_stable: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PathWriteRepairKind {
    Symlink,
    WriteStateid,
    TruncateStateid,
}

/// Independent merged READ results plus temporary-open cleanup bookkeeping.
pub struct PathReadOutcome {
    pub results: Vec<vfsi_core::OpResult<(Vec<u8>, bool)>>,
    pub opened: Vec<(NfsFh, stateid4)>,
    pub close_failed: Option<u32>,
}

// A protocol wave stops at the server's first failure; the public raw engine
// resumes its proven unexecuted suffix. These indices never escape as a
// completed-prefix promise in Vfsi.
struct PathReadWave {
    data: Vec<Option<Vec<u8>>>,
    eof: Vec<Option<bool>>,
    opened: Vec<(NfsFh, stateid4)>,
    failed: Option<(usize, u32)>,
    close_failed: Option<u32>,
    errors: Vec<Option<vfsi_core::OpError>>,
    fatal: Option<RpcError>,
    submitted: Vec<(usize, usize, usize)>,
    dispatched: bool,
}

/// One path-based GETATTR for a merged compound.
pub struct PathGetattrOp {
    pub file: FileRef,
    pub attrs: Vec<u32>,
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

// A private protocol continuation, never exposed as Vfsi execution progress.
type PathMutationRun<T> = (Vec<vfsi_core::OpResult<T>>, Option<PathRepair>);

/// Backend-only continuation at a parent/guard rejection, before later mutations.
#[derive(Clone, Copy)]
pub(crate) struct PathRepair {
    pub(crate) input: usize,
    pub(crate) failed_op: u32,
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
    pub opened: Vec<Option<(NfsFh, stateid4)>>,
    pub failed: Option<(usize, u32)>,
}

/// One path-based RENAME pair for a merged compound.
pub struct PathRenamePair {
    pub src: Vec<u8>,
    pub dst: Vec<u8>,
}

/// Compound-local "current filehandle" tracking, mirroring the txn-compound
/// client: the parent directory of the current batch is resolved once
/// (PUTROOTFH + LOOKUPs) and SAVEFH'd; child operations climb back to it with
/// RESTOREFH instead of re-resolving from the root.
#[derive(Default)]
struct CfhCursor {
    /// Root-relative path of the directory currently in the saved-fh slot.
    saved_dir: Option<Box<[u8]>>,
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
    fn set_parent<'p>(&mut self, c: &mut Compound, path: &'p [u8]) -> Option<(&'p [u8], usize)> {
        let (dir, leaf) = split_path_bytes(path).ok()?;
        let changed = self.saved_dir.as_deref() != Some(dir);
        let mut ops = self.walk_parent(c, dir);
        if changed {
            c.savefh();
            ops += 1;
            self.saved_dir = Some(dir.into());
            self.at_saved = true;
        }
        Some((leaf, ops))
    }

    /// Make the current fh the parent of `path` WITHOUT saving it (used by
    /// RENAME, which needs the saved-fh slot to keep the source directory).
    fn set_current_parent<'p>(
        &mut self,
        c: &mut Compound,
        path: &'p [u8],
    ) -> Option<(&'p [u8], usize)> {
        let (dir, leaf) = split_path_bytes(path).ok()?;
        Some((leaf, self.walk_parent(c, dir)))
    }

    // Move only the current handle. The caller explicitly decides whether
    // SAVEFH should replace the retained parent (RENAME must not replace it).
    fn walk_parent(&mut self, c: &mut Compound, dir: &[u8]) -> usize {
        if self.saved_dir.as_deref() == Some(dir) {
            let mut ops = 0;
            if !self.at_saved {
                c.restorefh();
                ops += 1;
                self.at_saved = true;
            }
            return ops;
        }
        let mut ops = 0;
        if let Some(saved) = self.saved_dir.as_deref() {
            let saved_comps = path_components(saved);
            let target_comps = path_components(dir);
            let common = saved_comps
                .clone()
                .zip(target_comps.clone())
                .take_while(|(saved, target)| saved == target)
                .count();
            let ups = saved_comps.count() - common;
            let target_count = target_comps.clone().count();
            let downs = target_count - common;
            if ups + downs < 1 + target_count {
                // Restore only for a walk relative to the saved directory.
                // A root walk replaces the current handle directly.
                if !self.at_saved {
                    c.restorefh();
                    ops += 1;
                    self.at_saved = true;
                }
                for _ in 0..ups {
                    c.lookupp();
                    ops += 1;
                }
                for comp in target_comps.skip(common) {
                    c.lookup(comp);
                    ops += 1;
                }
                self.at_saved = false;
                return ops;
            }
        }
        let mut ops = 1; // PUTROOTFH
        c.putrootfh();
        for comp in path_components(dir) {
            c.lookup(comp);
            ops += 1;
        }
        self.at_saved = false; // current fh differs from the saved one
        ops
    }

    /// Make the current fh a known open-file handle (descriptor ops). The
    /// saved fh is untouched, so a later path op can still RESTOREFH back.
    fn set_handle(&mut self, c: &mut Compound, fh: &NfsFh) {
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
fn validate_create_mode(mode: Option<u32>, set: &bitmap4) -> vfsi_core::OpResult<()> {
    if mode.is_some() && (set.bitmap4_len < 2 || set.map[1] & (1 << (FATTR4_MODE % 32)) == 0) {
        return Err(vfsi_core::OpError::unknown(
            vfsi_core::VfError::invalid_reply(None, "CREATE did not acknowledge requested mode"),
            None,
        ));
    }
    Ok(())
}

fn batch_is_read_only(tag: &[u8]) -> bool {
    matches!(
        tag,
        b"getattrv"
            | b"vstatfs_impl"
            | b"readlinkv"
            | b"readv"
            | b"lookup_typev"
            | b"readdir_pages"
            | b"readdir_children"
    )
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

pub(crate) fn collect_rpc_results<T>(
    result: vfsi_core::VecResult<T>,
    expected: usize,
) -> RpcResult<Vec<T>> {
    vfsi_core::internal::collect_vec_result(result, expected).map_err(|cause| {
        if let Some(vfsi_core::StatusCode::Nfs(status)) = cause.status() {
            RpcError::op(cause.index().unwrap_or(0), status)
        } else {
            RpcError::transport_with_kind(
                cause
                    .transport_kind()
                    .unwrap_or(vfsi_core::TransportKind::Other),
                cause.to_string(),
            )
        }
    })
}

fn remap_cleanup_error(error: RpcError, cleanup_indices: &[usize]) -> RpcError {
    if error.is_transport() {
        return error;
    }
    let op_index = error.op_index;
    error.with_op_index(cleanup_indices.get(op_index).copied().unwrap_or(op_index))
}

fn path_components(path: &[u8]) -> impl Iterator<Item = &[u8]> + Clone {
    path.split(|&byte| byte == b'/')
        .filter(|component| !component.is_empty())
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
    let report = map.analyze(res).map_err(|error| {
        RpcError::transport_with_kind(
            vfsi_core::TransportKind::InvalidReply,
            format!("malformed NFS COMPOUND reply: {error}"),
        )
    })?;
    if let Some(status) = report.compound_failure {
        return Err(RpcError::op(0, status));
    }
    Ok(report
        .failure
        .map(|failure| (failure.caller, failure.status)))
}

mod path_open_cleanup;
pub(crate) use path_open_cleanup::latest_path_open_closes;
use path_open_cleanup::record_path_open_owners;

#[cfg(all(test, feature = "test-faults"))]
mod session_recovery_tests;

impl NfsClient {
    #[cfg(test)]
    pub(crate) fn replace_test_rpc(&mut self, rpc: crate::rpc::RpcClient) -> crate::rpc::RpcClient {
        std::mem::replace(&mut self.session.rpc, rpc)
    }

    /// Inject one valid positive/zero short response in merged-path tests.
    #[cfg(feature = "test-faults")]
    pub fn test_short_merged_read_once(&mut self, count: usize) {
        self.short_merged_read_once = Some(count);
    }

    /// Limit the first merged wire WRITE while keeping its logical request intact.
    #[cfg(feature = "test-faults")]
    pub fn test_short_merged_write_once(&mut self, count: usize) {
        self.short_merged_write_once = Some(count);
    }

    /// Reject a selected merged logical item's wire WRITE after an accepted prefix.
    #[cfg(feature = "test-faults")]
    pub fn test_fail_merged_write_once(&mut self, caller: usize, part: usize) {
        self.fail_merged_write_once = Some((caller, part));
    }

    #[cfg(feature = "test-faults")]
    pub fn test_merged_short_pending(&self) -> bool {
        self.short_merged_read_once.is_some() || self.short_merged_write_once.is_some()
    }

    /// Inject one pre-dispatch NFS4ERR_RESOURCE response for this compound
    /// shape. Test-only: no request is sent and no mutation can have run.
    #[cfg(feature = "test-faults")]
    pub fn inject_resource_rejection_once(&mut self, tag: &[u8]) {
        self.reject_next_compound_tag = Some(Box::from(tag));
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

    pub(crate) fn ensure_usable(&self) -> RpcResult<()> {
        if self.quarantined_opens {
            return Err(RpcError::transport(
                "NFS identity quarantined after an OPEN lost its cleanup handle; reconnect",
            ));
        }
        self.session.ensure_usable()
    }

    pub(crate) fn needs_fresh_identity(&self) -> bool {
        self.quarantined_opens
    }

    pub(crate) fn quarantine_open_identity(&mut self) {
        self.quarantined_opens = true;
    }

    pub(crate) fn inherit_open_quarantine(&mut self, attempted: &Self) {
        if attempted.needs_fresh_identity() && self.session.shares_identity(&attempted.session) {
            self.quarantine_open_identity();
        }
    }

    pub(crate) fn close_path_or_defer(&mut self, closes: Vec<CloseOp>) {
        if closes.is_empty() {
            return;
        }
        #[cfg(feature = "test-faults")]
        if let Some(injector) = &self.fault_injector
            && injector
                .check(&OpenFaultPoint::BeforePathCloseBatch)
                .is_err()
        {
            self.deferred_path_closes.extend(closes);
            return;
        }
        match self.close_descriptors(&closes) {
            Ok(outcomes) => {
                for (close, outcome) in closes.into_iter().zip(outcomes) {
                    match outcome {
                        Ok(()) => {
                            #[cfg(feature = "test-faults")]
                            {
                                self.confirmed_path_closes += 1;
                            }
                        }
                        Err(_) => self.deferred_path_closes.push(close),
                    }
                }
            }
            Err(_) => self.deferred_path_closes.extend(closes),
        }
    }

    fn drain_deferred_path_closes(&mut self) -> RpcResult<()> {
        let mut remaining = std::mem::take(&mut self.deferred_path_closes).into_iter();
        while let Some(close) = remaining.next() {
            match self.close_cleanup_slot(&close.fh, &close.stateid, OwnerSlot::User) {
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

    fn next_public_open_owner_name(&mut self) -> Box<[u8]> {
        let id = self.next_public_open_owner_id;
        self.next_public_open_owner_id = id.wrapping_add(1);
        let suffix = id.to_string();
        let mut name = Vec::with_capacity(self.session.open_owner.name.len() + 1 + suffix.len());
        name.extend_from_slice(&self.session.open_owner.name);
        name.push(b'-');
        name.extend_from_slice(suffix.as_bytes());
        name.into_boxed_slice()
    }

    /// Keep newly acquired user OPEN owners distinct from the old identity's
    /// still-live state while replacement is being prepared.
    pub(crate) fn prepare_reconnect(&mut self, previous: &Self) {
        self.next_public_open_owner_id = previous.next_public_open_owner_id;
    }

    pub(crate) fn retire_replaced_connection(&mut self, previous: &Self, mut closes: Vec<CloseOp>) {
        if !self.session.shares_identity(&previous.session) {
            return;
        }
        closes.extend(previous.deferred_path_closes.iter().map(|close| CloseOp {
            fh: close.fh.clone(),
            stateid: close.stateid,
        }));
        self.close_path_or_defer(closes);
        self.session.retire_previous_session(&previous.session);
    }

    pub(crate) fn abandon(&mut self) {
        self.session.abandon();
    }

    pub(crate) fn shutdown(&mut self) -> RpcResult<()> {
        let cleanup = self.drain_deferred_path_closes();
        let shutdown = self.session.shutdown();
        cleanup.and(shutdown)
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
            &crate::rpc::NfsAuth::AuthSys,
        )
    }

    /// Connect and negotiate a minor version using explicit authentication.
    pub fn connect_with_authentication(
        host: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
        authentication: &crate::rpc::NfsAuth,
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
        authentication: &crate::rpc::NfsAuth,
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
            &crate::rpc::NfsAuth::AuthSys,
        )
    }

    /// Connect with an explicit minor version, timeouts, and authentication.
    pub fn connect_minor_with_authentication(
        host: &str,
        minorversion: u32,
        connect_timeout: Duration,
        request_timeout: Duration,
        authentication: &crate::rpc::NfsAuth,
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
        authentication: &crate::rpc::NfsAuth,
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
            crate::rpc::NfsAuth::AuthSys => 1024,
            #[cfg(feature = "rpcsec-gss")]
            crate::rpc::NfsAuth::RpcsecGss { .. } => 8192,
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
            quarantined_opens: false,
            next_public_open_owner_id: 0,
            #[cfg(feature = "test-faults")]
            fault_injector: None,
            #[cfg(feature = "test-faults")]
            confirmed_path_closes: 0,
            #[cfg(feature = "test-faults")]
            reject_next_compound_tag: None,
            #[cfg(feature = "test-faults")]
            short_merged_read_once: None,
            #[cfg(feature = "test-faults")]
            short_merged_write_once: None,
            #[cfg(feature = "test-faults")]
            fail_merged_write_once: None,
            #[cfg(feature = "test-faults")]
            fail_trailing_write_close_once: false,
            #[cfg(feature = "test-faults")]
            fail_inline_path_close_once: false,
            #[cfg(feature = "test-faults")]
            fail_merged_read_once: None,
            #[cfg(feature = "test-faults")]
            fail_merged_truncate_once: false,
        })
    }

    /// Minor version negotiated for this session.
    pub fn minorversion(&self) -> u32 {
        self.session.minorversion
    }

    pub(crate) fn set_telemetry(&mut self, telemetry: Option<crate::telemetry::Telemetry>) {
        self.session.rpc.telemetry = telemetry;
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
        readdir_response_limits(self.read_compound_bytes(), operations, usize::MAX)
    }

    pub fn root(&self) -> &NfsFh {
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
                    #[cfg(feature = "test-faults")]
                    let invalid_stateid = stateid4 {
                        seqid: 0,
                        other: [!0; 12],
                    };
                    #[cfg(feature = "test-faults")]
                    let stateid = if std::mem::take(&mut self.fail_inline_path_close_once) {
                        &invalid_stateid
                    } else {
                        &SPECIAL_STATEID
                    };
                    #[cfg(not(feature = "test-faults"))]
                    let stateid = &SPECIAL_STATEID;
                    target.compound.close(stateid.seqid, stateid);
                    target.map.note_ops(1);
                    *target.opened_path = None;
                }
                let (leaf, nops) = target.cursor.set_parent(&mut *target.compound, path)?;
                target.map.note_ops(nops);
                let seqid = target.base_seq.wrapping_add(*target.opens_in_chunk as u32);
                if open.truncate_create {
                    target.compound.open_claim_null_create_mode(
                        seqid,
                        open.access,
                        OPEN4_SHARE_DENY_NONE,
                        self.session.clientid,
                        &self.session.path_owner.name,
                        leaf,
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
                        make_open_how(open.create),
                        leaf,
                    );
                }
                *target.opens_in_chunk += 1;
                target.map.note_ops(1);
                // Pin the opened object before data or cleanup can fail.
                // Resolving its path again cannot recover an unlinked owner.
                target.compound.getfh();
                target.map.note_ops(1);
                *target.opened_path = Some(Box::from(path.as_slice()));
                *target.fh_at_opened = true;
                if open.truncate_existing {
                    #[cfg(feature = "test-faults")]
                    let invalid_stateid = stateid4 {
                        seqid: 0,
                        other: [!0; 12],
                    };
                    #[cfg(feature = "test-faults")]
                    let stateid = if std::mem::take(&mut self.fail_merged_truncate_once) {
                        &invalid_stateid
                    } else {
                        &SPECIAL_STATEID
                    };
                    #[cfg(not(feature = "test-faults"))]
                    let stateid = &SPECIAL_STATEID;
                    target.compound.setattr_with_stateid(None, Some(0), stateid);
                    target.map.note_ops(1);
                }
                Some(true)
            }
            FileRef::Handle(handle) => {
                if target.close_in_compound && target.opened_path.is_some() && *target.fh_at_opened
                {
                    #[cfg(feature = "test-faults")]
                    let invalid_stateid = stateid4 {
                        seqid: 0,
                        other: [!0; 12],
                    };
                    #[cfg(feature = "test-faults")]
                    let stateid = if std::mem::take(&mut self.fail_inline_path_close_once) {
                        &invalid_stateid
                    } else {
                        &SPECIAL_STATEID
                    };
                    #[cfg(not(feature = "test-faults"))]
                    let stateid = &SPECIAL_STATEID;
                    target.compound.close(stateid.seqid, stateid);
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
            dispatched,
            submitted,
        } = chunk;
        if allow_repack
            && self.merged_paths_need_repack(tag, one_item_ops, compound, map.ranges.len())?
        {
            return Ok(PathIoChunkResult::Repack {
                max_items: map.ranges.len() - 1,
            });
        }
        CompoundBudget::new(self.max_ops).ensure(compound)?;
        if !self.compound_request_fits(compound)? {
            return Err(RpcError::op(0, nfsstat4_NFS4ERR_REQ_TOO_BIG));
        }
        self.ensure_usable()?;
        self.session.path_owner.seqid = base_seq.wrapping_add(opens_in_chunk as u32);
        *dispatched = true;
        let response = self
            .call_compound_with_safety(compound, safety)
            .inspect_err(|_| {
                submitted.clone_from(&map.ranges);
            })?;
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
        self.ensure_usable()?;
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
        let reply = match self.session.compound(compound) {
            Ok(reply) => reply,
            Err(error) => {
                // A lost reply can hide the acquired stateid even for CLAIM_FH.
                // Do not keep that OPEN alive by reusing a stable client ID.
                if compound.has_open() {
                    self.quarantine_open_identity();
                }
                return Err(error);
            }
        };
        if reply.status() == nfsstat4_NFS4ERR_RETRY_UNCACHED_REP {
            // SEQUENCE success plus an uncached-reply status on the next
            // operation says nothing about the original request's effects.
            // Never let ordinary failed-item decoding replay its suffix.
            if compound.has_open() {
                self.quarantine_open_identity();
            }
            let index = (0..reply.nops())
                .find(|&index| reply.op_status(index) == nfsstat4_NFS4ERR_RETRY_UNCACHED_REP)
                .unwrap_or(0);
            return Err(RpcError::op(index, nfsstat4_NFS4ERR_RETRY_UNCACHED_REP));
        }
        if reply.status() != nfsstat4_NFS4_OK
            && compound.has_named_open()
            && compound.has_unaddressed_open(&reply)
        {
            self.quarantine_open_identity();
        }
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
    pub fn lookup(&mut self, dir: &NfsFh, name: &[u8]) -> RpcResult<NfsFh> {
        let mut c = Compound::new();
        c.tag(b"lookup");
        c.putfh(&dir.as_nfs_fh());
        c.lookup(name);
        c.getfh();
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(NfsFh::from_nfs_fh(res.getfh(3)))
    }

    /// Look up `name` below `dir`, returning the child handle and its
    /// FATTR4_TYPE in one compound (`[PUTFH, LOOKUP, GETFH, GETATTR]`).
    /// A symlink is returned as-is (type `NF4LNK`), so callers can follow it.
    pub fn lookup_getattr(&mut self, dir: &NfsFh, name: &[u8]) -> RpcResult<(NfsFh, u32)> {
        let mut c = Compound::new();
        c.tag(b"lookup_getattr");
        c.putfh(&dir.as_nfs_fh());
        c.lookup(name);
        c.getfh();
        c.getattr(&[FATTR4_TYPE]);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let fh = NfsFh::from_nfs_fh(res.getfh(3));
        let t = res.getattr_bytes(4);
        let ftype = if t.len() >= 4 {
            u32::from_be_bytes(t[0..4].try_into().unwrap())
        } else {
            0
        };
        Ok((fh, ftype))
    }

    /// Resolve independent children without discarding confirmed siblings.
    pub fn lookup_getattr_many(
        &mut self,
        ops: &[(NfsFh, Vec<u8>)],
    ) -> vfsi_core::VecResult<(NfsFh, u32)> {
        self.fixed_results::<4, _, _>(
            b"lookup_typev",
            ops,
            usize::MAX,
            |_| 0,
            |compound, (dir, name), _| {
                compound.putfh(&dir.as_nfs_fh());
                compound.lookup(name);
                compound.getfh();
                compound.getattr(&[FATTR4_TYPE]);
            },
            |reply, local, _| {
                let start = 1 + 4 * local;
                let bytes = reply.getattr(start + 3);
                if bytes.len() != 4 {
                    return Err(vfsi_core::OpError::unknown(
                        vfsi_core::VfError::invalid_reply(None, "invalid LOOKUP type attribute"),
                        None,
                    ));
                }
                Ok((
                    NfsFh::from_nfs_fh(reply.getfh(start + 2)),
                    u32::from_be_bytes(bytes.try_into().unwrap()),
                ))
            },
        )
    }

    /// Tolerantly LOOKUP each `(parent, name)` in as few compounds as
    /// possible (`[PUTFH, LOOKUP, GETFH]` per element), returning the per-path
    /// result. A failed LOOKUP (e.g. `NFS4ERR_NOENT`) is reported per path;
    /// only transport / compound-level failures abort the whole call. The
    /// returned error index, when one is produced, is the element's position
    /// in `ops`.
    pub fn lookup_many(&mut self, ops: &[(NfsFh, Vec<u8>)]) -> RpcResult<Vec<Result<NfsFh, u32>>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Option<Result<NfsFh, u32>>> = (0..ops.len()).map(|_| None).collect();
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
                RpcError::transport_with_kind(
                    vfsi_core::TransportKind::InvalidReply,
                    format!("malformed lookupv COMPOUND reply: {error}"),
                )
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
            for &(caller, start, _) in map.completed_ranges(&report) {
                out[caller] = Some(Ok(NfsFh::from_nfs_fh(res.getfh(start + 2))));
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
    pub fn resolve(&mut self, path: &[u8]) -> RpcResult<NfsFh> {
        let components: Vec<_> = path_components(path).collect();
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
            current = NfsFh::from_nfs_fh(res.getfh(2 + take));
            cursor += take;
        }
        Ok(current)
    }

    /// READ several `[PUTFH, READ]` pairs in as few compounds as possible.
    /// Each i-th read in a compound is at resop `2 + 2*i` (SEQUENCE, PUTFH,
    /// READ, PUTFH, READ, ...). The kernel nfsd does not serve multiple READ
    /// ops per compound correctly; use an nfs-ganesha server for this.
    /// Returns the data and the server's EOF flag per request, in request
    /// order.
    pub fn readv(&mut self, ops: &[ReadOp]) -> vfsi_core::VecResult<(Vec<u8>, bool)> {
        self.readv_decode(ops, |_, data, eof| Ok((data.to_vec(), eof)))
    }

    /// Decode READ replies into caller storage while the compound reply is
    /// still alive. `on_data` sees each wire operation in request order.
    pub fn readv_into(
        &mut self,
        ops: &[ReadOp],
        mut on_data: impl FnMut(usize, &[u8]) -> RpcResult<()>,
    ) -> vfsi_core::VecResult<(usize, bool)> {
        self.readv_decode(ops, |index, data, eof| {
            on_data(index, data)?;
            Ok((data.len(), eof))
        })
    }

    fn validate_read_length(actual: usize, requested: usize) -> RpcResult<()> {
        if actual > requested {
            Err(RpcError::transport_with_kind(
                vfsi_core::TransportKind::InvalidReply,
                "NFS READ reply exceeded requested count",
            ))
        } else {
            Ok(())
        }
    }

    fn record_expected_read_length(
        expected: &mut [u32; MAX_COMPOUND_OPS],
        count: &mut usize,
        requested: usize,
    ) -> RpcResult<()> {
        let slot = expected
            .get_mut(*count)
            .ok_or_else(|| RpcError::transport("too many READ operations in compound"))?;
        *slot = u32::try_from(requested)
            .map_err(|_| RpcError::transport("READ count exceeds protocol limit"))?;
        *count += 1;
        Ok(())
    }

    // One packing/validation engine; destinations are statically dispatched
    // while the reply storage is alive. Into reads never allocate owned data.
    fn readv_decode<R>(
        &mut self,
        ops: &[ReadOp],
        mut decode: impl FnMut(usize, &[u8], bool) -> RpcResult<R>,
    ) -> vfsi_core::VecResult<R> {
        let byte_budget = self.read_compound_bytes().saturating_sub(128);
        // Reject an individually unrepresentable request before dispatching
        // any sibling. Aggregate response limits still pack independent waves.
        if ops
            .iter()
            .any(|op| (op.count as usize).saturating_add(128) > byte_budget)
        {
            return Err(vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_REP_TOO_BIG));
        }
        self.fixed_results::<2, _, _>(
            b"readv",
            ops,
            byte_budget,
            |op| (op.count as usize).saturating_add(128),
            |compound, op, _| {
                compound.putfh(&op.fh.as_nfs_fh());
                compound.read(&op.stateid, op.offset, op.count);
            },
            |reply, local, original| {
                let ok = reply.read(2 + 2 * local);
                Self::validate_read_length(ok.data.data_len as usize, ops[original].count as usize)
                    .and_then(|()| decode(original, reply.read_data(2 + 2 * local), ok.eof != 0))
                    .map_err(|error| {
                        vfsi_core::OpError::unknown(vfsi_core::error_from_rpc(error, None), None)
                    })
            },
        )
    }

    fn validate_write_completion(
        count: u32,
        committed: u32,
        requested: usize,
    ) -> vfsi_core::OpResult<(u32, u32)> {
        // All client WRITE requests use FILE_SYNC4. RFC 8881 section 18.32.3
        // requires the acknowledgement to provide at least that durability.
        if count as usize > requested || committed != stable_how4_FILE_SYNC4 {
            Err(vfsi_core::OpError::unknown(
                vfsi_core::VfError::invalid_reply(None, "invalid NFS FILE_SYNC WRITE completion"),
                None,
            ))
        } else {
            Ok((count, committed))
        }
    }

    /// WRITE independent `[PUTFH, WRITE]` pairs in bounded compounds.
    /// Preserve successful replies and resume only the proven unexecuted
    /// suffix after a status error. Lost submitted replies are never replayed.
    pub fn writev(&mut self, ops: &[WriteOp<'_>]) -> vfsi_core::VecResult<(u32, u32)> {
        let byte_budget = self.max_request_arg_bytes().saturating_sub(128);
        let rejection = |op: &WriteOp<'_>| {
            if op.offset.checked_add(op.data.len() as u64).is_none() {
                Some(vfsi_core::VfError::errno(None, libc::EOVERFLOW as u32))
            } else if op.data.len().saturating_add(256) > byte_budget {
                Some(vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_REQ_TOO_BIG))
            } else {
                None
            }
        };
        if ops.iter().any(|op| rejection(op).is_some()) {
            // Allocate remapping only for rejected inputs; the common batch
            // borrows the original operations directly without copying payloads.
            let mut slots = vec![None; ops.len()];
            let mut valid = Vec::new();
            let mut indices = Vec::new();
            for (index, op) in ops.iter().enumerate() {
                if let Some(cause) = rejection(op) {
                    slots[index] = Some(Err(vfsi_core::OpError::not_attempted(cause)));
                } else {
                    indices.push(index);
                    valid.push(WriteOp {
                        fh: op.fh.clone(),
                        stateid: op.stateid,
                        offset: op.offset,
                        data: op.data,
                    });
                }
            }
            if !valid.is_empty() {
                let results = self.writev(&valid).unwrap_or_else(|cause| {
                    (0..valid.len())
                        .map(|_| Err(vfsi_core::OpError::not_attempted(cause.clone())))
                        .collect()
                });
                for (index, result) in indices.into_iter().zip(results) {
                    slots[index] = Some(result);
                }
            }
            return Ok(slots
                .into_iter()
                .map(|slot| slot.expect("every WRITE input assigned"))
                .collect());
        }
        self.fixed_results::<2, _, _>(
            b"writev",
            ops,
            byte_budget,
            |op| op.data.len().saturating_add(256),
            |compound, op, _| {
                compound.putfh(&op.fh.as_nfs_fh());
                compound.write(&op.stateid, op.offset, stable_how4_FILE_SYNC4, op.data);
            },
            |reply, local, original| {
                let ok = reply.write(2 + 2 * local);
                Self::validate_write_completion(ok.count, ok.committed, ops[original].data.len())
            },
        )
    }

    /// REMOVE sibling names in bounded compounds, preserving every item result.
    /// Consecutive REMOVEs share one PUTFH. Only a proven untouched suffix resumes
    /// after a status error; completed or ambiguous removals are never replayed.
    pub fn remove_many(&mut self, dir: &NfsFh, names: &[Vec<u8>]) -> vfsi_core::VecResult<()> {
        #[cfg(feature = "test-faults")]
        let injector = self.fault_injector.clone();
        self.path_mutation_results(
            b"removev",
            1,
            names.len(),
            |compound, _, index| {
                let name = &names[index];
                if name.is_empty() || name.contains(&0) || name.contains(&b'/') {
                    return Err(vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_INVAL));
                }
                let first = compound.op_count() == 0;
                if first {
                    compound.putfh(&dir.as_nfs_fh());
                }
                compound.remove(name);
                Ok(1 + usize::from(first))
            },
            |_, _, _| Ok(()),
            |_, _| false,
            |_index| {
                #[cfg(feature = "test-faults")]
                if let Some(injector) = &injector {
                    injector.check(&OpenFaultPoint::BeforeRemoveChunk {
                        first_name: names[_index].clone(),
                    })?;
                }
                Ok(())
            },
        )
        .map(|(results, _)| results)
    }

    /// OPEN a file below `dir`. `create` controls creation semantics. Returns
    /// the file handle and the open stateid.
    pub fn open(
        &mut self,
        dir: &NfsFh,
        name: &[u8],
        access: u32,
        create: OpenCreate,
    ) -> RpcResult<(NfsFh, stateid4)> {
        self.open_slot(dir, name, access, create, OwnerSlot::User, None)
    }

    /// Like [`open`](Self::open) but uses the path-op open owner, whose
    /// stateids never collide with caller-held descriptors.
    pub fn open_path(
        &mut self,
        dir: &NfsFh,
        name: &[u8],
        access: u32,
        create: OpenCreate,
    ) -> RpcResult<(NfsFh, stateid4)> {
        self.open_slot(dir, name, access, create, OwnerSlot::Path, None)
    }

    /// Open a caller-owned descriptor and apply exclusive-create metadata
    /// without borrowing the temporary pathname-I/O owner's state.
    pub(crate) fn open_with_mode(
        &mut self,
        dir: &NfsFh,
        name: &[u8],
        access: u32,
        create: OpenCreate,
        mode: u32,
    ) -> RpcResult<(NfsFh, stateid4)> {
        self.open_slot(
            dir,
            name,
            access,
            create,
            OwnerSlot::User,
            Some(mode & 0o7777),
        )
    }

    fn open_slot(
        &mut self,
        dir: &NfsFh,
        name: &[u8],
        access: u32,
        create: OpenCreate,
        slot: OwnerSlot,
        post_create_mode: Option<u32>,
    ) -> RpcResult<(NfsFh, stateid4)> {
        let (seqid, owner_name) = match slot {
            OwnerSlot::User => (0, self.next_public_open_owner_name()),
            OwnerSlot::Path => (
                self.session.path_owner.seqid,
                Box::from(self.session.path_owner.name.as_slice()),
            ),
        };
        let mut c = Compound::new();
        c.tag(b"open");
        c.putfh(&dir.as_nfs_fh());
        let openhow = make_open_how(create);
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
        let open_result = res.open(2);
        let stateid = open_result.stateid;
        let fh = NfsFh::from_nfs_fh(res.getfh(3));
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid = self.session.path_owner.seqid.wrapping_add(1);
        }
        if create == OpenCreate::Exclusive {
            let attrs = verifier_storage_attrs(&open_result.attrset);
            if post_create_mode.is_some() || attrs.mode || attrs.atime || attrs.mtime {
                let timestamp = current_timestamp();
                let update = verifier_cleanup_setattr(&fh, attrs, post_create_mode, timestamp);
                if let Err(error) = collect_rpc_results(self.setattr_many(&[update]), 1) {
                    self.close_open_cleanup(
                        vec![CloseOp {
                            fh: fh.clone(),
                            stateid,
                        }],
                        slot,
                    );
                    return Err(error);
                }
            }
        }
        Ok((fh, stateid))
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
        add: impl FnMut(&mut Compound, &T, usize),
        extract: impl FnMut(&CompoundRes, usize) -> RpcResult<R>,
    ) -> RpcResult<Vec<R>> {
        self.batch_ops_fallible_with_failure(tag, per_op, ops, add, extract, |_, _| {})
    }

    fn batch_ops_fallible_with_failure<T, R>(
        &mut self,
        tag: &[u8],
        per_op: usize,
        ops: &[T],
        mut add: impl FnMut(&mut Compound, &T, usize),
        mut extract: impl FnMut(&CompoundRes, usize) -> RpcResult<R>,
        mut on_failure: impl FnMut(&mut Self, &[R]),
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
            let chunk_size = match self.op_budget_for(tag, 1 + per_op).batch_capacity(per_op) {
                Ok(chunk_size) => chunk_size,
                Err(error) => {
                    on_failure(self, &out);
                    return Err(error);
                }
            };
            let chunk = &ops[global..(global + chunk_size.min(max_items)).min(ops.len())];
            let chunk_start = global;
            let mut c = Compound::new();
            c.tag(tag);
            for op in chunk {
                add(&mut c, op, global);
                global += 1;
            }
            let fits = match self.compound_request_fits(&c) {
                Ok(fits) => fits,
                Err(error) => {
                    on_failure(self, &out);
                    return Err(error);
                }
            };
            if !fits && chunk.len() > 1 {
                max_items = chunk.len() / 2;
                global = chunk_start;
                continue;
            }
            max_items = usize::MAX;
            let res = match self.call_compound(&mut c) {
                Ok(res) => res,
                Err(error) => {
                    on_failure(self, &out);
                    if error.is_transport() {
                        return Err(error);
                    }
                    let idx = caller_index(error.op_index, per_op, chunk_start, chunk.len());
                    return Err(error.with_op_index(idx));
                }
            };
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
                    let completed = i.saturating_sub(1) / per_op;
                    for local in 0..completed.min(chunk.len()) {
                        out.push(extract(&res, local)?);
                    }
                    on_failure(self, &out);
                    let idx = caller_index(i, per_op, chunk_start, chunk.len());
                    return Err(RpcError::op(idx, res.op_status(i)));
                }
                None if res.status() != nfsstat4_NFS4_OK => {
                    let present = res.nops().saturating_sub(1);
                    let completed = (present / per_op).min(chunk.len());
                    for local in 0..completed {
                        out.push(extract(&res, local)?);
                    }
                    on_failure(self, &out);
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
    ) -> vfsi_core::VecResult<ReturnedAttrs> {
        self.fixed_results::<2, _, _>(
            b"vstatfs_impl",
            ops,
            usize::MAX,
            |_| 0,
            |compound, op, _| {
                compound.putfh(&op.fh.as_nfs_fh());
                compound.getattr(&op.attrs);
            },
            |reply, local, _| Ok(reply.getattr_with_bitmap(2 + 2 * local)),
        )
    }
    /// SETATTR mode and/or size on several files in one compound.
    pub fn setattr_many(&mut self, ops: &[SetattrOp]) -> vfsi_core::VecResult<()> {
        self.fixed_results::<2, _, _>(
            b"setattrv",
            ops,
            usize::MAX,
            |_| 0,
            |compound, op, _| {
                compound.putfh(&op.fh.as_nfs_fh());
                compound.setattr_ownership(
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
            |_, _, _| Ok(()),
        )
    }

    /// READLINK several files in as few compounds as possible.
    pub fn readlink_many(&mut self, ops: &[ReadlinkOp]) -> vfsi_core::VecResult<Vec<u8>> {
        self.fixed_results::<2, _, _>(
            b"readlinkv",
            ops,
            usize::MAX,
            |_| 0,
            |compound, op, _| {
                compound.putfh(&op.fh.as_nfs_fh());
                compound.readlink();
            },
            |reply, local, _| Ok(reply.readlink(2 + 2 * local).to_vec()),
        )
    }

    /// RENAME several pairs in as few compounds as possible. Each pair is
    /// `[PUTFH src, SAVEFH, PUTFH dst, RENAME]`.
    pub fn rename_many(&mut self, ops: &[RenameOp]) -> vfsi_core::VecResult<()> {
        self.fixed_results::<4, _, _>(
            b"renamev",
            ops,
            usize::MAX,
            |_| 0,
            |compound, op, _| {
                compound.putfh(&op.srcdir.as_nfs_fh());
                compound.savefh();
                compound.putfh(&op.dstdir.as_nfs_fh());
                compound.rename(&op.oldname, &op.newname);
            },
            |_, _, _| Ok(()),
        )
    }

    /// CREATE several objects (mkdir / symlink) in as few compounds as
    /// possible. CREATE changes the current filehandle, so each gets its own
    /// `[PUTFH dir, CREATE]`.
    pub fn create_many(&mut self, ops: &[CreateOp]) -> vfsi_core::VecResult<()> {
        self.fixed_results::<2, _, _>(
            b"createv",
            ops,
            usize::MAX,
            |_| 0,
            |compound, op, _| {
                compound.putfh(&op.dir.as_nfs_fh());
                compound.create(&op.name, op.ftype, op.linkdata.as_deref(), op.mode);
            },
            |reply, local, input| {
                validate_create_mode(ops[input].mode, reply.create_attrs(2 + 2 * local))
            },
        )
    }

    /// LINK independent sources without replaying completed mutations.
    /// Each input is PUTFH, SAVEFH, PUTFH, LINK.
    pub fn link_many(&mut self, ops: &[LinkOp]) -> vfsi_core::VecResult<()> {
        self.fixed_results::<4, _, _>(
            b"linkv",
            ops,
            usize::MAX,
            |_| 0,
            |compound, op, _| {
                compound.putfh(&op.src.as_nfs_fh());
                compound.savefh();
                compound.putfh(&op.dstdir.as_nfs_fh());
                compound.link(&op.newname);
            },
            |_, _, _| Ok(()),
        )
    }

    /// OPEN several files in as few compounds as possible; each is
    /// `[PUTFH dir, OPEN, GETFH]`. Open-owner seqids are assigned
    /// consecutively across the batch.
    pub fn open_many(&mut self, ops: &[OpenOp]) -> RpcResult<Vec<(NfsFh, stateid4)>> {
        self.open_many_slot(ops, OwnerSlot::User)
    }

    /// Like [`open_many`](Self::open_many) but using the path-op open owner.
    pub fn open_many_path(&mut self, ops: &[OpenOp]) -> RpcResult<Vec<(NfsFh, stateid4)>> {
        self.open_many_slot(ops, OwnerSlot::Path)
    }

    fn open_many_slot(
        &mut self,
        ops: &[OpenOp],
        slot: OwnerSlot,
    ) -> RpcResult<Vec<(NfsFh, stateid4)>> {
        let (base, owner_name) = match slot {
            OwnerSlot::User => (0, Box::<[u8]>::default()),
            OwnerSlot::Path => (
                self.session.path_owner.seqid,
                Box::from(self.session.path_owner.name.as_slice()),
            ),
        };
        let n = ops.len();
        let public_owner_names: Vec<Box<[u8]>> = if matches!(slot, OwnerSlot::User) {
            (0..n).map(|_| self.next_public_open_owner_name()).collect()
        } else {
            Vec::new()
        };
        let clientid = self.session.clientid;
        let out = self.batch_ops_fallible_with_failure(
            b"openv",
            3,
            ops,
            |c, op, gi| {
                c.putfh(&op.dir.as_nfs_fh());
                let openhow = make_open_how(op.create);
                let (seqid, owner) = if matches!(slot, OwnerSlot::User) {
                    (0, &public_owner_names[gi])
                } else {
                    (base.wrapping_add(gi as u32), &owner_name)
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
                let open_result = res.open(2 + 3 * i);
                // i is compound-local. Decode independently of the original
                // request; cleanup below checks its global creation mode.
                let attrs = verifier_storage_attrs(&open_result.attrset);
                let fh = res.getfh(3 + 3 * i);
                Ok((NfsFh::from_nfs_fh(fh), open_result.stateid, attrs))
            },
            |client, opened: &[(NfsFh, stateid4, VerifierStorageAttrs)]| {
                let timestamp = current_timestamp();
                let cleanup_indices: Vec<usize> = opened
                    .iter()
                    .enumerate()
                    .filter_map(|(index, (_, _, attrs))| {
                        (ops[index].create == OpenCreate::Exclusive
                            && (attrs.mode || attrs.atime || attrs.mtime))
                            .then_some(index)
                    })
                    .collect();
                if !cleanup_indices.is_empty() {
                    let cleanup: Vec<SetattrOp> = cleanup_indices
                        .iter()
                        .map(|&index| {
                            verifier_cleanup_setattr(
                                &opened[index].0,
                                opened[index].2,
                                None,
                                timestamp,
                            )
                        })
                        .collect();
                    let _ = client.setattr_many(&cleanup);
                }
                if matches!(slot, OwnerSlot::Path) {
                    client.session.path_owner.seqid = base.wrapping_add(opened.len() as u32);
                }
                let closes: Vec<CloseOp> = opened
                    .iter()
                    .map(|(fh, stateid, _)| CloseOp {
                        fh: fh.clone(),
                        stateid: *stateid,
                    })
                    .collect();
                client.close_open_cleanup(closes, slot);
            },
        )?;
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid = base.wrapping_add(n as u32);
        }
        let timestamp = current_timestamp();
        let cleanup_indices: Vec<usize> = out
            .iter()
            .enumerate()
            .filter_map(|(index, (_, _, attrs))| {
                (ops[index].create == OpenCreate::Exclusive
                    && (attrs.mode || attrs.atime || attrs.mtime))
                    .then_some(index)
            })
            .collect();
        if !cleanup_indices.is_empty() {
            let cleanup: Vec<SetattrOp> = cleanup_indices
                .iter()
                .map(|&index| {
                    verifier_cleanup_setattr(&out[index].0, out[index].2, None, timestamp)
                })
                .collect();
            if let Err(error) = collect_rpc_results(self.setattr_many(&cleanup), cleanup.len()) {
                let error = remap_cleanup_error(error, &cleanup_indices);
                let closes: Vec<CloseOp> = out
                    .iter()
                    .map(|(fh, stateid, _)| CloseOp {
                        fh: fh.clone(),
                        stateid: *stateid,
                    })
                    .collect();
                self.close_open_cleanup(closes, slot);
                return Err(error);
            }
        }
        Ok(out
            .into_iter()
            .map(|(fh, stateid, _)| (fh, stateid))
            .collect())
    }

    /// Independent descriptor CLOSE outcomes. Resume only the proven
    /// unexecuted suffix after a protocol failure; never replay a closed input.
    pub(crate) fn close_descriptors(&mut self, ops: &[CloseOp]) -> vfsi_core::VecResult<()> {
        self.fixed_results::<2, _, _>(
            b"closev",
            ops,
            usize::MAX,
            |_| 0,
            |compound, op, _| {
                compound.putfh(&op.fh.as_nfs_fh());
                compound.close(1, &op.stateid);
            },
            |_, _, _| Ok(()),
        )
    }

    /// CLOSE public descriptors in as few compounds as possible. Each has an
    /// independent OPEN owner, so each CLOSE uses seqid 1.
    pub fn close_many(&mut self, ops: &[CloseOp]) -> RpcResult<()> {
        self.close_many_slot(ops, OwnerSlot::User)
    }

    /// Like [`close_many`](Self::close_many) but using the path-op open owner.
    pub fn close_many_path(&mut self, ops: &[CloseOp]) -> RpcResult<()> {
        self.close_many_cleanup_slot(ops, OwnerSlot::Path)
    }

    fn close_many_cleanup_slot(&mut self, ops: &[CloseOp], slot: OwnerSlot) -> RpcResult<()> {
        #[cfg(feature = "test-faults")]
        if let Some(injector) = &self.fault_injector
            && let Err(error) = injector.check(&OpenFaultPoint::BeforePathCloseBatch)
        {
            return Err(RpcError::transport(error.to_string()));
        }
        self.close_many_slot(ops, slot)?;
        #[cfg(feature = "test-faults")]
        {
            self.confirmed_path_closes += ops.len();
        }
        Ok(())
    }

    // Repairs use an independent temporary owner. The original compound may
    // already have closed its special stateid; separate-close callers may
    // still own their path OPENs. Neither ownership may be borrowed here.
    fn with_path_io_repair_target<T, E>(
        &mut self,
        file: &FileRef,
        stateid: Option<&stateid4>,
        access: u32,
        caller: usize,
        mut map_error: impl FnMut(RpcError) -> E,
        repair: impl FnOnce(&mut Self, &NfsFh, &stateid4) -> Result<T, E>,
    ) -> Result<T, E> {
        match file {
            FileRef::Handle(fh) => repair(self, fh, stateid.unwrap_or(&SPECIAL_STATEID)),
            FileRef::Path(path) => {
                let (parent, name) = split_path_bytes(path)
                    .map_err(|_| map_error(RpcError::op(caller, nfsstat4_NFS4ERR_INVAL)))?;
                let parent = self
                    .resolve(parent)
                    .map_err(|error| map_error(error.with_op_index(caller)))?;
                let (fh, stateid) = self
                    .open(&parent, name, access, OpenCreate::NoCreate)
                    .map_err(|error| map_error(error.with_op_index(caller)))?;
                let result = repair(self, &fh, &stateid);
                self.close_path_or_defer(vec![CloseOp { fh, stateid }]);
                result
            }
        }
    }

    fn repair_path_read(
        &mut self,
        op: &PathReadOp,
        caller: usize,
        offset: u64,
        count: usize,
        output: &mut Vec<u8>,
    ) -> RpcResult<bool> {
        self.with_path_io_repair_target(
            &op.file,
            op.stateid.as_ref(),
            OPEN4_SHARE_ACCESS_READ,
            caller,
            std::convert::identity,
            |client, fh, stateid| {
                complete_read_suffix(
                    client,
                    caller,
                    offset,
                    count,
                    |client, offset, count| client.read(fh, stateid, offset, count as u32),
                    |data| output.extend_from_slice(data),
                )
            },
        )
    }

    fn repair_path_write(
        &mut self,
        ops: &[PathWriteOp<'_>],
        later: &[(usize, usize, usize)],
        caller: usize,
        part: usize,
        data: &[u8],
    ) -> vfsi_core::OpResult<u32> {
        let op = &ops[caller];
        let offset =
            checked_offset(op.offset, part, caller).map_err(io_progress::write_rpc_error)?;
        self.with_path_io_repair_target(
            &op.file,
            op.stateid.as_ref(),
            OPEN4_SHARE_ACCESS_WRITE,
            caller,
            io_progress::write_rpc_error,
            |client, fh, stateid| {
                // Known same-file conflicts are split by WriteWaveAccess.
                // A hard-link alias can only be discovered on this slow path.
                // Never repair an earlier gap through a later accepted write.
                let end = checked_offset(offset, data.len(), caller)
                    .map_err(io_progress::write_rpc_error)?;
                for &(index, _, _) in later {
                    if index <= caller {
                        continue;
                    }
                    let next = &ops[index];
                    let overlaps = next.truncate
                        || (next.offset < end
                            && offset
                                < checked_offset(next.offset, next.data.len(), index)
                                    .map_err(io_progress::write_rpc_error)?);
                    if !overlaps {
                        continue;
                    }
                    let alias = match &next.file {
                        FileRef::Handle(handle) => handle == fh,
                        FileRef::Path(path) => {
                            client.resolve(path).map_err(io_progress::write_rpc_error)? == *fh
                        }
                    };
                    if alias {
                        return Err(io_progress::write_rpc_error(RpcError::op(
                            caller,
                            nfsstat4_NFS4ERR_IO,
                        )));
                    }
                }
                complete_write_suffix(client, offset, data, |client, offset, data| {
                    client.ensure_usable().map_err(|error| {
                        vfsi_core::OpError::not_attempted(vfsi_core::error_from_rpc(error, None))
                    })?;
                    client
                        .write(fh, stateid, offset, data)
                        .map_err(io_progress::write_rpc_error)
                })
            },
        )
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
    /// (Ganesha resolves it to the current open). A final GETFH retains the
    /// pinned owner for close-only recovery if that CLOSE fails. Otherwise the
    /// separate-close form returns stateids/filehandles for a
    /// follow-up compound (the portable fallback).
    ///
    /// Each input keeps its own result and confirmed WRITE prefix. A known
    /// unexecuted suffix resumes without replaying completed inputs. Path references may be resolved again across compound
    /// boundaries or short-I/O repair; use Handle references when object identity
    /// must remain pinned across concurrent namespace changes.
    pub fn writev_path_compound(
        &mut self,
        ops: &[PathWriteOp<'_>],
        close_in_compound: bool,
    ) -> vfsi_core::VfResult<PathWriteOutcome> {
        self.writev_path_run(ops, close_in_compound, false)
            .map(|(outcome, _)| outcome)
    }

    // Native namespace/special-stateid recovery must finish before any later
    // WRITE can overwrite an alias. Yield only protocol-proven rejections;
    // the public raw engine still completes every independent input.
    pub(crate) fn writev_path_run(
        &mut self,
        ops: &[PathWriteOp<'_>],
        mut close_in_compound: bool,
        recover_path_errors: bool,
    ) -> vfsi_core::VfResult<(PathWriteOutcome, Option<PathWriteRepair>)> {
        use vfsi_core::{FailureState, IoProgress, OpError, error_from_rpc};
        let n = ops.len();
        let reject = |op: &PathWriteOp<'_>| {
            if op.offset.checked_add(op.data.len() as u64).is_none() {
                Some(vfsi_core::VfError::errno(None, libc::EOVERFLOW as u32))
            } else if matches!(&op.file, FileRef::Path(path) if split_path_bytes(path).is_err()) {
                Some(vfsi_core::VfError::errno(None, libc::EISDIR as u32))
            } else {
                None
            }
        };
        if ops.iter().any(|op| reject(op).is_some()) {
            let mut slots = vec![None; n];
            let mut valid = Vec::new();
            let mut indices = Vec::new();
            for (index, op) in ops.iter().enumerate() {
                if let Some(cause) = reject(op) {
                    slots[index] = Some(Err(OpError::not_attempted(cause)));
                } else {
                    indices.push(index);
                    valid.push(PathWriteOp {
                        file: match &op.file {
                            FileRef::Path(path) => FileRef::Path(path.clone()),
                            FileRef::Handle(fh) => FileRef::Handle(fh.clone()),
                        },
                        offset: op.offset,
                        data: op.data,
                        create: op.create,
                        truncate: op.truncate,
                        stateid: op.stateid,
                    });
                }
            }
            let mut opened = Vec::new();
            let mut close_failed = None;
            let mut repair = None;
            if !valid.is_empty() {
                let results =
                    match self.writev_path_run(&valid, close_in_compound, recover_path_errors) {
                        Ok((outcome, pending)) => {
                            repair = pending.map(|mut repair| {
                                repair.index = indices[repair.index];
                                repair
                            });
                            opened = outcome.opened;
                            close_failed = outcome.close_failed;
                            outcome.results
                        }
                        Err(cause) => (0..valid.len())
                            .map(|_| Err(OpError::not_attempted(cause.clone())))
                            .collect(),
                    };
                for (index, result) in indices.into_iter().zip(results) {
                    slots[index] = Some(result);
                }
            }
            return Ok((
                PathWriteOutcome {
                    results: slots
                        .into_iter()
                        .map(|slot| slot.expect("every merged WRITE assigned"))
                        .collect(),
                    opened,
                    close_failed,
                },
                repair,
            ));
        }
        let mut counts: Vec<Option<usize>> = vec![None; n];
        let mut committed: Vec<Option<u32>> = vec![None; n];
        let mut opened: Vec<(NfsFh, stateid4)> = Vec::new();
        let mut errors: Vec<Option<OpError>> = vec![None; n];
        let mut submitted = Vec::new();
        let mut dispatched = false;
        let mut repair = None;
        let mut close_failed: Option<u32> = None;
        // GETFH follows OPEN even in full compounds, retaining cleanup identity.
        // Headroom for a new directory's path resolution inside a compound.
        let reserve = 8;
        let per_op = self.per_op_bytes();

        let operation = (|| -> RpcResult<()> {
            let mut global = 0usize;
            // Bytes of ops[global] already emitted across earlier compounds
            // (>0 means ops[global] is being continued mid-file).
            let mut part_off = 0usize;
            let mut max_items = usize::MAX;
            while global < n {
                submitted.clear();
                let mut failed: Option<(usize, u32)> = None;
                let per_file = if close_in_compound { 5 } else { 4 };
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
                let mut opened_path: Option<Box<[u8]>> = None;
                let mut fh_at_opened = false;
                let mut opens_in_chunk = 0usize;
                let base_seq = self.session.path_owner.seqid;
                let mut payload = 0usize;
                let mut expected_writes = [(0usize, 0usize); MAX_COMPOUND_OPS];
                let mut expected_write_count = 0;
                #[cfg(feature = "test-faults")]
                let mut injected_short_write = false;
                let mut active_files = std::collections::HashMap::new();

                while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                    if errors[global].is_some() {
                        global += 1;
                        part_off = 0;
                        continue;
                    }
                    let op = &ops[global];
                    let start = part_off;
                    let range = op.offset..checked_offset(op.offset, op.data.len(), global)?;
                    match active_files.entry(&op.file) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(WriteWaveAccess::new(range, !op.truncate));
                        }
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            if !entry.get_mut().admit(range, !op.truncate) {
                                break;
                            }
                        }
                    }
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
                            create: if op.create && start == 0 {
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
                            let requested = chunk.len();
                            let slot = expected_writes
                                .get_mut(expected_write_count)
                                .ok_or_else(|| RpcError::transport("too many merged WRITEs"))?;
                            *slot = (start + off, requested);
                            expected_write_count += 1;
                            #[cfg(feature = "test-faults")]
                            let chunk = if !injected_short_write {
                                if let Some(count) = self.short_merged_write_once {
                                    injected_short_write = true;
                                    &chunk[..count.min(chunk.len())]
                                } else {
                                    chunk
                                }
                            } else {
                                chunk
                            };
                            #[cfg(feature = "test-faults")]
                            let invalid_stateid = stateid4 {
                                seqid: 0,
                                other: [!0; 12],
                            };
                            #[cfg(feature = "test-faults")]
                            let stateid = if self.fail_merged_write_once
                                == Some((global, (start + off) / per_op))
                            {
                                self.fail_merged_write_once = None;
                                &invalid_stateid
                            } else {
                                stateid
                            };
                            c.write(
                                stateid,
                                checked_offset(op.offset, start + off, global)?,
                                stable_how4_FILE_SYNC4,
                                chunk,
                            );
                            map.note_ops(1);
                            off += requested;
                        }
                    } else {
                        // A zero-length write still emits a WRITE for this item.
                        let slot = expected_writes
                            .get_mut(expected_write_count)
                            .ok_or_else(|| RpcError::transport("too many merged WRITEs"))?;
                        *slot = (start, 0);
                        expected_write_count += 1;
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
                    if global == n {
                        break;
                    }
                    errors[chunk_start] = Some(OpError::not_attempted(vfsi_core::VfError::nfs(
                        None,
                        nfsstat4_NFS4ERR_TOO_MANY_OPS,
                    )));
                    global = chunk_start + 1;
                    part_off = 0;
                    continue;
                }
                // The OPEN-adjacent GETFH already pinned this owner.
                let needs_trailing_close = close_in_compound && opened_path.is_some();
                let trailing_close = if needs_trailing_close {
                    let index = map.next;
                    if map.next < self.max_ops {
                        #[cfg(feature = "test-faults")]
                        let invalid_stateid = stateid4 {
                            seqid: 0,
                            other: [!0; 12],
                        };
                        #[cfg(feature = "test-faults")]
                        let close_stateid =
                            if std::mem::take(&mut self.fail_trailing_write_close_once) {
                                &invalid_stateid
                            } else {
                                &SPECIAL_STATEID
                            };
                        #[cfg(not(feature = "test-faults"))]
                        let close_stateid = &SPECIAL_STATEID;
                        c.close(close_stateid.seqid, close_stateid);
                        map.note_ops(1);
                        Some(index)
                    } else {
                        None
                    }
                } else {
                    None
                };
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
                    dispatched: &mut dispatched,
                    submitted: &mut submitted,
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
                record_path_open_owners(&res, &mut opened);
                let report = map.analyze(&res).map_err(|error| {
                    submitted.clone_from(&map.ranges);
                    RpcError::transport_with_kind(
                        vfsi_core::TransportKind::InvalidReply,
                        format!("malformed writev COMPOUND reply: {error}"),
                    )
                })?;
                // Cleanup of the preceding owner runs before this input.
                let inline_close_failure = report
                    .failure
                    .filter(|failure| res.op(failure.op_index).resop == nfs_opnum4_NFS4_OP_CLOSE);
                let range_failed = report
                    .failure
                    .filter(|_| inline_close_failure.is_none())
                    .map(|failure| (failure.caller, failure.status));
                if let Some((caller, _status)) = range_failed {
                    global = caller + 1;
                    part_off = 0;
                    for i in caller + 1..n {
                        counts[i] = None;
                        committed[i] = None;
                    }
                }
                // Decode every successful WRITE, including the prefix of the
                // logical input whose later wire operation failed.
                let mut write_index = 0;
                for (caller, start, end) in map
                    .ranges
                    .iter()
                    .filter(|(_, start, _)| *start < res.nops())
                {
                    for index in *start..(*end).min(res.nops()) {
                        if res.op(index).resop != nfs_opnum4_NFS4_OP_WRITE
                            || res.op_status(index) != 0
                        {
                            continue;
                        }
                        let ok = res.write(index);
                        #[cfg(feature = "test-faults")]
                        if injected_short_write {
                            self.short_merged_write_once = None;
                        }
                        let (part, requested) = *expected_writes[..expected_write_count]
                            .get(write_index)
                            .ok_or_else(|| RpcError::transport("extra merged WRITE reply"))?;
                        write_index += 1;
                        if errors[*caller].is_some() {
                            continue;
                        }
                        let received = ok.count as usize;
                        if let Err(error) =
                            Self::validate_write_completion(ok.count, ok.committed, requested)
                        {
                            errors[*caller] = Some(error);
                            continue;
                        }
                        if counts[*caller].unwrap_or(0) != part {
                            errors[*caller] = Some(OpError::unknown(
                                vfsi_core::VfError::invalid_reply(
                                    None,
                                    "noncontiguous merged WRITE completion",
                                ),
                                None,
                            ));
                            continue;
                        }
                        *counts[*caller].get_or_insert(0) += received;
                        committed[*caller] = Some(
                            committed[*caller]
                                .unwrap_or(stable_how4_FILE_SYNC4)
                                .min(ok.committed),
                        );
                        if received == 0 && requested != 0 {
                            errors[*caller] = Some(OpError::failed(
                                vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_IO),
                                None,
                            ));
                            continue;
                        }
                        if received < requested {
                            #[cfg(feature = "test-faults")]
                            if let Some(injector) = &self.fault_injector
                                && let Err(cause) = injector
                                    .check(&OpenFaultPoint::BeforeWriteRepair { index: *caller })
                            {
                                errors[*caller] = Some(OpError::failed(cause, None));
                                continue;
                            }
                            match self.repair_path_write(
                                ops,
                                &map.ranges[..report.completed.len()
                                    + usize::from(report.failure.is_some())],
                                *caller,
                                part + received,
                                &ops[*caller].data[part + received..part + requested],
                            ) {
                                Ok(stable) => {
                                    *counts[*caller]
                                        .as_mut()
                                        .expect("acknowledged WRITE prefix") +=
                                        requested - received;
                                    committed[*caller] =
                                        Some(committed[*caller].unwrap().min(stable));
                                }
                                Err(error) => {
                                    let (cause, state, progress) = error.into_parts();
                                    if let Some(IoProgress::Write { bytes_written }) = progress {
                                        *counts[*caller].as_mut().unwrap() += bytes_written;
                                    }
                                    errors[*caller] = Some(if state == FailureState::Unknown {
                                        OpError::unknown(cause, None)
                                    } else {
                                        OpError::failed(cause, None)
                                    });
                                }
                            }
                        }
                    }
                }
                if let Some((caller, status)) = range_failed
                    && errors[caller].is_none()
                {
                    errors[caller] =
                        Some(OpError::failed(vfsi_core::VfError::nfs(None, status), None));
                }
                // The trailing CLOSE (close_in_compound form) sits outside any
                // file range; report it without replaying acknowledged WRITEs.
                // Subsequent calls may use the separate-close form.
                let close_status = if let Some(failure) = inline_close_failure {
                    // Only cleanup ran in this range. Resume this untouched
                    // input after closing or retaining the prior owner.
                    global = failure.caller;
                    part_off = 0;
                    Some(failure.status)
                } else if let Some(status) = report.compound_failure {
                    if trailing_close.is_some() && report.compound_failure_op == trailing_close {
                        Some(status)
                    } else {
                        return Err(RpcError::op(0, status));
                    }
                } else if needs_trailing_close && trailing_close.is_none() {
                    Some(nfsstat4_NFS4ERR_TOO_MANY_OPS)
                } else {
                    None
                };
                if let Some(status) = close_status {
                    close_failed = Some(status);
                    self.close_path_or_defer(latest_path_open_closes(&opened));
                    opened.clear();
                    close_in_compound = false;
                }
                if recover_path_errors
                    && let Some(failure) = report.failure
                    && matches!(ops[failure.caller].file, FileRef::Path(_))
                    && errors[failure.caller].as_ref().is_some_and(|error| {
                        error.state() == FailureState::Failed
                            && error.cause().status()
                                == Some(vfsi_core::StatusCode::Nfs(failure.status))
                    })
                {
                    let failed_op = res.op(failure.op_index).resop;
                    let kind = if failure.status == nfsstat4_NFS4ERR_SYMLINK
                        && matches!(
                            failed_op,
                            nfs_opnum4_NFS4_OP_OPEN
                                | nfs_opnum4_NFS4_OP_LOOKUP
                                | nfs_opnum4_NFS4_OP_LOOKUPP
                        ) {
                        Some(PathWriteRepairKind::Symlink)
                    } else if is_stateid_error(failure.status) {
                        match failed_op {
                            nfs_opnum4_NFS4_OP_WRITE => Some(PathWriteRepairKind::WriteStateid),
                            nfs_opnum4_NFS4_OP_SETATTR
                                if ops[failure.caller].truncate
                                    && !ops[failure.caller].create
                                    && counts[failure.caller].unwrap_or(0) == 0 =>
                            {
                                Some(PathWriteRepairKind::TruncateStateid)
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        repair = Some(PathWriteRepair {
                            index: failure.caller,
                            kind,
                            prefix_stable: committed[failure.caller]
                                .unwrap_or(stable_how4_FILE_SYNC4)
                                == stable_how4_FILE_SYNC4,
                        });
                        break;
                    }
                }
                if chunk_start == global && part_off == 0 {
                    // No progress (capacity check failed even for one op): bail.
                    errors[chunk_start].get_or_insert_with(|| {
                        OpError::not_attempted(vfsi_core::VfError::nfs(
                            None,
                            nfsstat4_NFS4ERR_TOO_MANY_OPS,
                        ))
                    });
                    global = chunk_start + 1;
                }
            }
            Ok(())
        })();
        let fatal = operation.err().map(|error| error_from_rpc(error, None));
        if !dispatched && let Some(cause) = fatal {
            return Err(cause);
        }
        if fatal.is_some() && !opened.is_empty() {
            self.close_path_or_defer(latest_path_open_closes(&opened));
            opened.clear();
        }
        let results = counts
            .into_iter()
            .zip(committed)
            .zip(errors)
            .enumerate()
            .map(|(index, ((count, committed), error))| {
                let confirmed = count.unwrap_or(0);
                if error.is_none() && count.is_some() && confirmed == ops[index].data.len() {
                    return Ok((confirmed, committed.unwrap_or(stable_how4_FILE_SYNC4)));
                }
                let error = error.unwrap_or_else(|| {
                    let cause = fatal
                        .clone()
                        .unwrap_or_else(|| vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_IO));
                    if fatal.as_ref().is_some_and(uncertain_completion)
                        && submitted.iter().any(|&(caller, _, _)| caller == index)
                    {
                        OpError::unknown(cause, None)
                    } else if count.is_some() {
                        OpError::failed(cause, None)
                    } else {
                        OpError::not_attempted(cause)
                    }
                });
                Err(io_progress::add_write_prefix(
                    error,
                    confirmed,
                    ops[index].data.len(),
                ))
            })
            .collect();
        Ok((
            PathWriteOutcome {
                results,
                opened,
                close_failed,
            },
            repair,
        ))
    }

    /// Batched path-based READs in one compound per chunk, same shape as
    /// [`writev_path_compound`](Self::writev_path_compound) but with READ and
    /// no creation. Path references may be resolved again across compound
    /// boundaries or short-I/O repair; Handle references keep the opened object.
    /// Execute all independent path reads, retaining one outcome per input.
    /// Only proven unexecuted suffixes are submitted after a known item failure.
    pub fn readv_path_compound(
        &mut self,
        ops: &[PathReadOp],
        close_in_compound: bool,
    ) -> vfsi_core::VfResult<PathReadOutcome> {
        use vfsi_core::{FailureState, IoProgress, OpError, error_from_rpc};
        let mut results = Vec::with_capacity(ops.len());
        let mut opened = Vec::new();
        let mut close_failed = None;
        let mut start = 0;
        while start < ops.len() {
            let wave = self.read_path_wave(&ops[start..], close_in_compound);
            if !wave.dispatched
                && results.is_empty()
                && let Some(error) = wave.fatal
            {
                return Err(error_from_rpc(error, None));
            }
            let fatal = wave.fatal.map(|error| error_from_rpc(error, None));
            let end = if fatal.is_none() && wave.close_failed.is_none() {
                wave.failed
                    .map_or(ops.len() - start, |(index, _)| index + 1)
            } else {
                ops.len() - start
            };
            let fallback = fatal.clone().unwrap_or_else(|| {
                vfsi_core::VfError::nfs(
                    None,
                    wave.failed
                        .map(|(_, status)| status)
                        .or(wave.close_failed)
                        .unwrap_or(nfsstat4_NFS4ERR_IO),
                )
            });
            for (index, ((data, eof), error)) in wave
                .data
                .into_iter()
                .zip(wave.eof)
                .zip(wave.errors)
                .chain(std::iter::repeat_with(|| ((None, None), None)))
                .take(end)
                .enumerate()
            {
                let received = data.is_some();
                let data = data.unwrap_or_default();
                let eof = eof.unwrap_or(false);
                let complete = received && (data.len() >= ops[start + index].count || eof);
                if error.is_none() && complete {
                    results.push(Ok((data, eof)));
                    continue;
                }
                let error = error.unwrap_or_else(|| {
                    let submitted = wave.submitted.iter().any(|&(caller, _, _)| caller == index);
                    if fatal.as_ref().is_some_and(uncertain_completion) && submitted {
                        OpError::unknown(fallback.clone(), None)
                    } else if data.is_empty() {
                        OpError::not_attempted(fallback.clone())
                    } else {
                        OpError::failed(fallback.clone(), None)
                    }
                });
                let (cause, mut state, _) = error.into_parts();
                if !data.is_empty() && state == FailureState::NotAttempted {
                    state = FailureState::Failed;
                }
                let progress = (!data.is_empty()).then(|| {
                    IoProgress::Read(vfsi_core::api::ReadResult::owned(
                        ops[start + index].offset,
                        data,
                        eof,
                    ))
                });
                results.push(Err(match state {
                    FailureState::NotAttempted => OpError::not_attempted(cause),
                    FailureState::Failed => OpError::failed(cause, progress),
                    FailureState::Unknown => OpError::unknown(cause, progress),
                }));
            }
            opened.extend(wave.opened);
            close_failed = wave.close_failed;
            start += end;
        }
        Ok(PathReadOutcome {
            results,
            opened,
            close_failed,
        })
    }

    fn read_path_wave(&mut self, ops: &[PathReadOp], mut close_in_compound: bool) -> PathReadWave {
        let n = ops.len();
        let mut data: Vec<Option<Vec<u8>>> = Vec::new();
        let mut eof: Vec<Option<bool>> = Vec::new();
        let mut opened: Vec<(NfsFh, stateid4)> = Vec::new();
        let mut failed: Option<(usize, u32)> = None;
        let mut close_failed: Option<u32> = None;
        let reserve = 8;
        let per_op = self.read_per_op_bytes();

        let mut errors = Vec::new();
        let mut submitted = Vec::new();
        let mut dispatched = false;
        let operation = (|| -> RpcResult<()> {
            let mut global = 0usize;
            // Bytes of ops[global] already fetched across earlier compounds
            // (>0 means ops[global] is being continued mid-file).
            let mut part_off = 0usize;
            let mut max_items = usize::MAX;
            while global < n {
                submitted.clear();
                self.ensure_usable()?;
                let per_file = if close_in_compound { 5 } else { 4 };
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
                let mut opened_path: Option<Box<[u8]>> = None;
                let mut fh_at_opened = false;
                let mut opens_in_chunk = 0usize;
                let base_seq = self.session.path_owner.seqid;
                let mut payload = 0usize;
                let mut expected_read_lengths = [0; MAX_COMPOUND_OPS];
                let mut expected_read_count = 0usize;

                while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                    if data.len() <= global {
                        data.resize_with(global + 1, || None);
                        eof.resize_with(global + 1, || None);
                        errors.resize_with(global + 1, || None);
                    }
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
                    #[cfg(feature = "test-faults")]
                    let invalid_stateid = stateid4 {
                        seqid: 0,
                        other: [!0; 12],
                    };
                    #[cfg(feature = "test-faults")]
                    let stateid = if self.fail_merged_read_once == Some(global) {
                        self.fail_merged_read_once = None;
                        &invalid_stateid
                    } else {
                        stateid
                    };
                    if end > start {
                        let mut off = 0usize;
                        for chunk_len in chunk_lens(start, end, per_op) {
                            Self::record_expected_read_length(
                                &mut expected_read_lengths,
                                &mut expected_read_count,
                                chunk_len,
                            )?;
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
                        Self::record_expected_read_length(
                            &mut expected_read_lengths,
                            &mut expected_read_count,
                            0,
                        )?;
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
                if failed.is_none()
                    && self.merged_paths_need_repack(tag, 1 + per_file, &c, map.ranges.len())?
                {
                    max_items = map.ranges.len() - 1;
                    global = chunk_start;
                    part_off = chunk_part_off;
                    continue;
                }
                CompoundBudget::new(self.max_ops).ensure(&c)?;
                if !self.compound_request_fits(&c)? {
                    return Err(RpcError::op(chunk_start, nfsstat4_NFS4ERR_REQ_TOO_BIG));
                }
                self.session.path_owner.seqid = base_seq.wrapping_add(opens_in_chunk as u32);
                submitted = map.ranges.clone();
                dispatched = true;
                let res = self.call_compound_with_safety(&mut c, RequestSafety::ReadOnly)?;
                if resource_rejected_before_mutation(&res) {
                    self.session.path_owner.seqid = base_seq;
                    if self.can_retry_merged_resource(tag, &res, budget, reserve, per_file) {
                        max_items = usize::MAX;
                        global = chunk_start;
                        part_off = chunk_part_off;
                        continue;
                    }
                }
                max_items = usize::MAX;
                record_path_open_owners(&res, &mut opened);
                let report = map.analyze(&res).map_err(|error| {
                    RpcError::transport_with_kind(
                        vfsi_core::TransportKind::InvalidReply,
                        format!("malformed readv COMPOUND reply: {error}"),
                    )
                })?;
                // Cleanup of the preceding owner runs before this input.
                let inline_close_failure = report
                    .failure
                    .filter(|failure| res.op(failure.op_index).resop == nfs_opnum4_NFS4_OP_CLOSE);
                let range_failed = report
                    .failure
                    .filter(|_| inline_close_failure.is_none())
                    .map(|failure| (failure.caller, failure.status));
                if let Some((caller, st)) = range_failed {
                    failed = Some((caller, st));
                }
                let mut read_index = 0usize;
                for (caller, s, e) in map
                    .ranges
                    .iter()
                    .filter(|(_, start, _)| *start < res.nops())
                {
                    for j in *s..(*e).min(res.nops()) {
                        let ro = res.op(j);
                        if ro.resop == nfs_opnum4_NFS4_OP_READ && res.op_status(j) == 0 {
                            // SAFETY: the successful READ owns data_len initialized bytes in
                            // the live CompoundRes; requested lengths are checked before
                            // copying or decoding.
                            let ok = res.read(j);
                            #[allow(unused_mut)]
                            let mut len = ok.data.data_len as usize;
                            #[allow(unused_mut)]
                            let mut end_of_file = ok.eof != 0;
                            #[cfg(feature = "test-faults")]
                            if let Some(count) = self.short_merged_read_once.take()
                                && count < len
                            {
                                len = count;
                                end_of_file = false;
                            }
                            let requested = match expected_read_lengths[..expected_read_count]
                                .get(read_index)
                                .copied()
                            {
                                Some(requested) => requested,
                                None => {
                                    let error = RpcError::transport(
                                        "malformed readv COMPOUND reply: extra READ result",
                                    )
                                    .with_op_index(*caller);
                                    errors[*caller] = Some(vfsi_core::OpError::unknown(
                                        vfsi_core::error_from_rpc(error, None),
                                        None,
                                    ));
                                    continue;
                                }
                            };
                            read_index += 1;
                            if errors[*caller].is_some() {
                                continue;
                            }
                            if let Err(error) = Self::validate_read_length(len, requested as usize)
                            {
                                errors[*caller] = Some(vfsi_core::OpError::unknown(
                                    vfsi_core::error_from_rpc(error, None),
                                    None,
                                ));
                                continue;
                            }
                            // Borrow the XDR reply while it is alive and
                            // copy once into the final growing result buffer.
                            let bytes = &res.read_data(j)[..len];
                            if eof[*caller] == Some(true) {
                                continue;
                            }
                            let output = data[*caller].get_or_insert_with(Vec::new);
                            output.extend_from_slice(bytes);
                            if len == 0 && requested != 0 && !end_of_file {
                                errors[*caller] = Some(vfsi_core::OpError::failed(
                                    vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_IO),
                                    None,
                                ));
                                continue;
                            }
                            if len < requested as usize && !end_of_file {
                                #[cfg(feature = "test-faults")]
                                if let Some(injector) = &self.fault_injector
                                    && let Err(cause) = injector
                                        .check(&OpenFaultPoint::BeforeReadRepair { index: *caller })
                                {
                                    errors[*caller] = Some(vfsi_core::OpError::failed(cause, None));
                                    continue;
                                }
                                let offset =
                                    checked_offset(ops[*caller].offset, output.len(), *caller)?;
                                match self.repair_path_read(
                                    &ops[*caller],
                                    *caller,
                                    offset,
                                    requested as usize - len,
                                    output,
                                ) {
                                    Ok(end) => end_of_file = end,
                                    Err(error) => {
                                        let cause = vfsi_core::error_from_rpc(error, None);
                                        errors[*caller] = Some(if uncertain_completion(&cause) {
                                            vfsi_core::OpError::unknown(cause, None)
                                        } else {
                                            vfsi_core::OpError::failed(cause, None)
                                        });
                                    }
                                }
                            }
                            eof[*caller] = Some(end_of_file);
                        }
                    }
                }
                if let Some((caller, status)) = range_failed
                    && errors[caller].is_none()
                {
                    errors[caller] = Some(vfsi_core::OpError::failed(
                        vfsi_core::VfError::nfs(None, status),
                        None,
                    ));
                }
                if let Some(failure) = inline_close_failure {
                    self.close_path_or_defer(latest_path_open_closes(&opened));
                    opened.clear();
                    close_in_compound = false;
                    global = failure.caller;
                    part_off = 0;
                    continue;
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
                // The builder may have left the final input mid-request.
                // EOF or a per-input failure ends that input, not its siblings.
                if part_off != 0 && (eof[global] == Some(true) || errors[global].is_some()) {
                    global += 1;
                    part_off = 0;
                }
                if chunk_start == global && part_off == 0 {
                    failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                    break;
                }
            }
            Ok(())
        })();
        let fatal = operation.err();
        if fatal.is_some() && !opened.is_empty() {
            self.close_path_or_defer(latest_path_open_closes(&opened));
            opened.clear();
        }
        PathReadWave {
            data,
            eof,
            opened,
            failed,
            close_failed,
            errors,
            fatal,
            submitted,
            dispatched,
        }
    }

    /// Batched path-based GETATTRs in one compound per chunk:
    /// `[SEQUENCE, PUTROOTFH, LOOKUP <parent>, SAVEFH, LOOKUP, GETATTR,
    /// RESTOREFH, LOOKUP, GETATTR, ...]`.
    pub fn getattr_path_compound(
        &mut self,
        ops: &[PathGetattrOp],
    ) -> vfsi_core::VecResult<Vec<u8>> {
        self.getattr_path_with_bitmap(ops).map(|results| {
            results
                .into_iter()
                .map(|result| result.map(|(_, bytes)| bytes))
                .collect()
        })
    }

    pub(crate) fn getattr_path_with_bitmap(
        &mut self,
        ops: &[PathGetattrOp],
    ) -> vfsi_core::VecResult<ReturnedAttrs> {
        use vfsi_core::{OpError, VfError, error_from_rpc};
        if !ops.is_empty() {
            self.ensure_usable()
                .map_err(|error| error_from_rpc(error, None))?;
        }
        let mut slots: Vec<Option<vfsi_core::OpResult<ReturnedAttrs>>> =
            (0..ops.len()).map(|_| None).collect();
        let per_file = 4;
        let reserve = 16;
        let mut global = 0;
        let mut max_items = usize::MAX;
        let mut dispatched = false;
        while global < ops.len() {
            let budget = self
                .op_budget_for(b"getattrv1", 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"getattrv1");
            let mut payload = 0usize;
            while global < ops.len()
                && map.ranges.len() < max_items
                && map.next + per_file <= budget
            {
                if slots[global].is_some() {
                    global += 1;
                    continue;
                }
                let op = &ops[global];
                let estimate = 256;
                if payload > 0
                    && self.max_request_arg_bytes() > 0
                    && payload + estimate > self.max_request_arg_bytes()
                {
                    break;
                }
                map.begin(global);
                match &op.file {
                    FileRef::Path(path) => {
                        let Some((leaf, count)) = cursor.set_parent(&mut c, path) else {
                            map.ranges.pop();
                            slots[global] = Some(Err(OpError::not_attempted(VfError::nfs(
                                None,
                                nfsstat4_NFS4ERR_INVAL,
                            ))));
                            global += 1;
                            break;
                        };
                        map.note_ops(count);
                        c.lookup(leaf);
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
                payload += estimate;
                global += 1;
            }
            if map.ranges.is_empty() {
                if global > chunk_start {
                    continue;
                }
                let error = VfError::nfs(None, nfsstat4_NFS4ERR_TOO_MANY_OPS);
                if !dispatched {
                    return Err(error);
                }
                return Ok(completion::finish_slots(slots, &[], error));
            }
            match self.merged_paths_need_repack(b"getattrv1", 1 + per_file, &c, map.ranges.len()) {
                Ok(true) => {
                    max_items = map.ranges.len() - 1;
                    global = chunk_start;
                    continue;
                }
                Ok(false) => {}
                Err(error) => {
                    let error = error_from_rpc(error, None);
                    if !dispatched {
                        return Err(error);
                    }
                    return Ok(completion::finish_slots(slots, &[], error));
                }
            }
            if map.ranges.len() == 1 {
                let fits = CompoundBudget::new(self.max_ops)
                    .ensure(&c)
                    .and_then(|()| self.compound_request_fits(&c));
                if !matches!(fits, Ok(true)) {
                    let cause = fits
                        .err()
                        .map(|error| error_from_rpc(error, None))
                        .unwrap_or_else(|| VfError::nfs(None, nfsstat4_NFS4ERR_REQ_TOO_BIG));
                    if !dispatched {
                        return Err(cause);
                    }
                    return Ok(completion::finish_slots(slots, &[], cause));
                }
            }
            max_items = usize::MAX;
            dispatched = true;
            let res = match self.call_compound_with_safety(&mut c, RequestSafety::ReadOnly) {
                Ok(res) => res,
                Err(error) => {
                    return Ok(completion::finish_slots(
                        slots,
                        &map.ranges,
                        error_from_rpc(error, None),
                    ));
                }
            };
            let report = match map.analyze(&res) {
                Ok(report) => report,
                Err(error) => {
                    return Ok(completion::finish_slots(
                        slots,
                        &map.ranges,
                        VfError::invalid_reply(None, format!("malformed metadata reply: {error}")),
                    ));
                }
            };
            for &(input, start, end) in map.completed_ranges(&report) {
                let result = (start..end)
                    .find(|&index| res.op(index).resop == nfs_opnum4_NFS4_OP_GETATTR)
                    .map(|index| res.getattr_with_bitmap(index))
                    .ok_or_else(|| {
                        OpError::unknown(
                            VfError::invalid_reply(None, "missing completed GETATTR payload"),
                            None,
                        )
                    });
                slots[input] = Some(result);
            }
            if let Some(failure) = report.failure {
                if self.can_retry_read_only_merged_resource(
                    b"getattrv1",
                    &res,
                    budget,
                    reserve,
                    per_file,
                ) {
                    global = failure.caller;
                } else {
                    slots[failure.caller] = Some(Err(OpError::failed(
                        VfError::nfs(None, failure.status),
                        None,
                    )));
                    global = failure.caller + 1;
                }
            } else if let Some(status) = report.compound_failure {
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
                // A compound-level rejection before any item executed.
                return Ok(completion::finish_slots(
                    slots,
                    &[],
                    VfError::nfs(None, status),
                ));
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing merged metadata outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    /// Independent path-based SETATTR outcomes; optional types are returned
    /// only for successful logical inputs. Mutations are never replayed after
    /// ambiguous replies or ordinary SETATTR failures.
    pub fn setattr_path_compound(
        &mut self,
        ops: &[PathSetattrOp],
    ) -> vfsi_core::VecResult<Option<u32>> {
        self.setattr_path_compound_inner(ops, None)
            .map(|(results, _)| results)
    }

    /// Yield before later mutations when a path guard requires repair. The
    /// continuation is protocol bookkeeping, not a public prefix guarantee.
    pub(crate) fn setattr_path_compound_guarded(
        &mut self,
        ops: &[PathSetattrOp],
        guards: &[bool],
    ) -> vfsi_core::VfResult<PathMutationRun<Option<u32>>> {
        if ops.len() != guards.len() {
            return Err(vfsi_core::VfError::from_kind(
                None,
                std::io::ErrorKind::InvalidInput,
            ));
        }
        self.setattr_path_compound_inner(ops, Some(guards))
    }

    fn setattr_path_compound_inner(
        &mut self,
        ops: &[PathSetattrOp],
        guards: Option<&[bool]>,
    ) -> vfsi_core::VfResult<PathMutationRun<Option<u32>>> {
        use vfsi_core::{OpError, VfError};
        self.path_mutation_results(
            b"setattrv1",
            5,
            ops.len(),
            |compound, cursor, index| {
                let op = &ops[index];
                let mut count = match &op.file {
                    FileRef::Path(path) => {
                        let (name, count) = cursor
                            .set_parent(compound, path)
                            .ok_or_else(|| VfError::nfs(None, nfsstat4_NFS4ERR_INVAL))?;
                        compound.lookup(name);
                        count + 1
                    }
                    FileRef::Handle(fh) => {
                        cursor.set_handle(compound, fh);
                        1
                    }
                };
                if guards.is_some_and(|guards| guards[index]) {
                    compound.nverify_type(nfs_ftype4_NF4LNK);
                    count += 1;
                } else if guards.is_none() && op.check_type {
                    compound.getattr(&[FATTR4_TYPE]);
                    count += 1;
                }
                compound.setattr_ownership(
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
                Ok(count + 1)
            },
            |reply, start, end| {
                (start..end)
                    .find(|&index| reply.op(index).resop == nfs_opnum4_NFS4_OP_GETATTR)
                    .map(|index| {
                        let bytes = reply.getattr(index);
                        if bytes.len() != 4 {
                            Err(OpError::unknown(
                                VfError::invalid_reply(None, "invalid SETATTR type attribute"),
                                None,
                            ))
                        } else {
                            Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
                        }
                    })
                    .transpose()
            },
            |status, op| {
                guards.is_some()
                    && (status == nfsstat4_NFS4ERR_SAME && op == nfs_opnum4_NFS4_OP_NVERIFY
                        || status == nfsstat4_NFS4ERR_SYMLINK
                            && matches!(op, nfs_opnum4_NFS4_OP_LOOKUP | nfs_opnum4_NFS4_OP_LOOKUPP))
            },
            |_| Ok(()),
        )
    }

    /// Batched OPENs addressing retained objects, with one independent owner
    /// per open instance. Confirmed prefixes remain owned on any failure.
    pub(crate) fn openv_handle_compound(
        &mut self,
        ops: &[(NfsFh, u32)],
    ) -> RpcResult<PathOpenOutcome> {
        let owners: Vec<_> = (0..ops.len())
            .map(|_| self.next_public_open_owner_name())
            .collect();
        let mut opened = vec![None; ops.len()];
        let mut global = 0;
        let mut max_items = usize::MAX;
        let attempt = (|| -> RpcResult<Option<(usize, u32)>> {
            while global < ops.len() {
                let budget = self.op_budget_for(b"reopenfh", 3).merged_limit(0, 2);
                let start = global;
                let mut map = ExecutionMap::new();
                let mut c = Compound::new();
                c.tag(b"reopenfh");
                while global < ops.len() && map.ranges.len() < max_items && map.next + 2 <= budget {
                    map.begin(global);
                    c.putfh(&ops[global].0.as_nfs_fh());
                    c.open_claim_fh(ops[global].1, self.session.clientid, &owners[global]);
                    map.note_ops(2);
                    map.end();
                    global += 1;
                }
                if map.ranges.is_empty() {
                    return Ok(Some((start, nfsstat4_NFS4ERR_TOO_MANY_OPS)));
                }
                if self.merged_paths_need_repack(b"reopenfh", 3, &c, map.ranges.len())? {
                    max_items = map.ranges.len() - 1;
                    global = start;
                    continue;
                }
                let res =
                    self.call_compound_with_safety(&mut c, RequestSafety::NonIdempotentMutation)?;
                if resource_rejected_before_mutation(&res)
                    && self.can_retry_merged_resource(b"reopenfh", &res, budget, 0, 2)
                {
                    global = start;
                    continue;
                }
                let failed = first_failed_range(&res, &map)?;
                for &(caller, begin, _) in &map.ranges {
                    if failed.is_some_and(|(index, _)| caller >= index) {
                        break;
                    }
                    opened[caller] = Some((ops[caller].0.clone(), res.open(begin + 1).stateid));
                }
                if failed.is_some() {
                    return Ok(failed);
                }
                max_items = usize::MAX;
            }
            Ok(None)
        })();
        match attempt {
            Ok(failed) => Ok(PathOpenOutcome { opened, failed }),
            Err(error) => {
                let closes = opened
                    .into_iter()
                    .flatten()
                    .map(|(fh, stateid)| CloseOp { fh, stateid })
                    .collect();
                self.close_path_or_defer(closes);
                Err(error)
            }
        }
    }

    /// Batched path-based REMOVEs in one compound per chunk. REMOVE keeps
    /// the current fh on the parent, so same-directory removals chain.
    pub fn removev_path_compound(&mut self, paths: &[Vec<u8>]) -> vfsi_core::VecResult<()> {
        self.remove_path_results(paths, false)
            .map(|(results, _)| results)
    }

    pub(crate) fn remove_path_results(
        &mut self,
        paths: &[Vec<u8>],
        repair_ancestors: bool,
    ) -> vfsi_core::VfResult<PathMutationRun<()>> {
        self.path_mutation_results(
            b"removev1",
            3,
            paths.len(),
            |compound, cursor, index| {
                if paths[index].contains(&0) {
                    return Err(vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_INVAL));
                }
                split_path_bytes(&paths[index])
                    .map_err(|_| vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_INVAL))?;
                let (name, count) = cursor
                    .set_parent(compound, &paths[index])
                    .expect("validated REMOVE path");
                compound.remove(name);
                Ok(count + 1)
            },
            |_, _, _| Ok(()),
            |status, op| {
                repair_ancestors
                    && matches!(status, nfsstat4_NFS4ERR_SYMLINK | nfsstat4_NFS4ERR_NOTDIR)
                    && matches!(
                        op,
                        nfs_opnum4_NFS4_OP_LOOKUP
                            | nfs_opnum4_NFS4_OP_LOOKUPP
                            | nfs_opnum4_NFS4_OP_REMOVE
                    )
            },
            |_| Ok(()),
        )
    }

    /// Batched path-based RENAMEs in one compound per chunk. Each pair is
    /// `[.. SAVEFH src-dir, .., RENAME]`; RENAME uses the saved fh as the
    /// source directory and the current fh as the destination directory.
    pub fn renamev_path_compound(&mut self, pairs: &[PathRenamePair]) -> vfsi_core::VecResult<()> {
        self.rename_path_results(pairs, false)
            .map(|(results, _)| results)
    }

    pub(crate) fn rename_path_results(
        &mut self,
        pairs: &[PathRenamePair],
        repair_ancestors: bool,
    ) -> vfsi_core::VfResult<PathMutationRun<()>> {
        self.path_mutation_results(
            b"renamev1",
            8,
            pairs.len(),
            |compound, cursor, index| {
                let pair = &pairs[index];
                // Validate BOTH names before either directory walk appends operations.
                // A rejected destination must not leave orphaned source operations.
                split_path_bytes(&pair.src)
                    .map_err(|_| vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_INVAL))?;
                split_path_bytes(&pair.dst)
                    .map_err(|_| vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_INVAL))?;
                let (source, source_ops) = cursor
                    .set_parent(compound, &pair.src)
                    .expect("validated source");
                let (target, target_ops) = cursor
                    .set_current_parent(compound, &pair.dst)
                    .expect("validated target");
                compound.rename(source, target);
                Ok(source_ops + target_ops + 1)
            },
            |_, _, _| Ok(()),
            |status, op| {
                // RFC 8881 sections 15.1.2 and 18.26.3: these parent-type
                // rejections cannot perform the rename. A server may detect a
                // symlink parent in LOOKUP/LOOKUPP or at RENAME itself.
                repair_ancestors
                    && matches!(status, nfsstat4_NFS4ERR_SYMLINK | nfsstat4_NFS4ERR_NOTDIR)
                    && matches!(
                        op,
                        nfs_opnum4_NFS4_OP_LOOKUP
                            | nfs_opnum4_NFS4_OP_LOOKUPP
                            | nfs_opnum4_NFS4_OP_RENAME
                    )
            },
            |_| Ok(()),
        )
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
                    base.wrapping_add(gi as u32)
                };
                c.close(seqid, &op.stateid);
            },
            |_, _| (),
        )?;
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid = base.wrapping_add(n as u32);
        }
        Ok(())
    }

    /// READ `count` bytes at `offset`; returns the data read and the server's
    /// EOF flag.
    pub fn read(
        &mut self,
        fh: &NfsFh,
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
        Self::validate_read_length(len, count as usize)?;
        let data = res.read_data(2).to_vec();
        Ok((data, ok.eof != 0))
    }

    /// WRITE `data` at `offset` with FILE_SYNC stability; returns bytes
    /// written and committed.
    pub fn write(
        &mut self,
        fh: &NfsFh,
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
        Self::validate_write_completion(ok.count, ok.committed, data.len()).map_err(|error| {
            RpcError::transport_with_kind(vfsi_core::TransportKind::InvalidReply, error.to_string())
        })
    }

    /// Copy bytes entirely on an NFSv4.2 server. The source filehandle is
    /// saved and the destination is current as required by RFC 7862.
    pub fn copy(&mut self, op: &CopyOp) -> RpcResult<u64> {
        collect_rpc_results(self.copy_many(std::slice::from_ref(op)), 1)
            .map(|mut results| results.remove(0))
    }

    /// Issue multiple COPY operations in as few compounds as the negotiated
    /// operation limit permits.
    pub fn copy_many(&mut self, ops: &[CopyOp]) -> vfsi_core::VecResult<u64> {
        use vfsi_core::OpError;
        let mut prepared = Vec::with_capacity(ops.len());
        let mut rejected = Vec::new();
        for (index, op) in ops.iter().enumerate() {
            if op.src_fh == op.dst_fh
                || op.count != 0
                    && (op.src_offset.checked_add(op.count).is_none()
                        || op.dst_offset.checked_add(op.count).is_none())
            {
                rejected.push((
                    index,
                    OpError::not_attempted(vfsi_core::VfError::nfs(None, nfsstat4_NFS4ERR_INVAL)),
                ));
            } else {
                prepared.push((index, op));
            }
        }
        let result = self.fixed_results::<4, _, _>(
            b"copyv",
            &prepared,
            usize::MAX,
            |_| 0,
            |compound, (_, op), _| {
                compound.putfh(&op.src_fh.as_nfs_fh());
                compound.savefh();
                compound.putfh(&op.dst_fh.as_nfs_fh());
                compound.copy(
                    &op.src_stateid,
                    &op.dst_stateid,
                    op.src_offset,
                    op.dst_offset,
                    op.count,
                );
            },
            |reply, local, input| {
                validate_copy_result(prepared[input].1, reply.copy(4 + 4 * local))
            },
        );
        if rejected.is_empty() {
            return result;
        }
        let results = match result {
            Ok(results) => results,
            Err(cause) => prepared
                .iter()
                .map(|_| Err(OpError::not_attempted(cause.clone())))
                .collect(),
        };
        let mut slots = (0..ops.len()).map(|_| None).collect::<Vec<_>>();
        for (index, error) in rejected {
            slots[index] = Some(Err(error));
        }
        for ((index, _), result) in prepared.into_iter().zip(results) {
            slots[index] = Some(result);
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("every COPY input assigned"))
            .collect())
    }

    /// CLOSE the open file.
    pub fn close(&mut self, fh: &NfsFh, stateid: &stateid4) -> RpcResult<()> {
        self.close_slot(fh, stateid, OwnerSlot::User)
    }

    /// Like [`close`](Self::close) but using the path-op open owner.
    pub fn close_path(&mut self, fh: &NfsFh, stateid: &stateid4) -> RpcResult<()> {
        self.close_cleanup_slot(fh, stateid, OwnerSlot::Path)
    }

    /// Close a temporary shared-owner OPEN once; retain unconfirmed cleanup.
    pub(crate) fn close_temporary_path(&mut self, fh: &NfsFh, stateid: &stateid4) -> RpcResult<()> {
        let result = self.close_path(fh, stateid);
        if result.is_err() {
            self.deferred_path_closes.push(CloseOp {
                fh: fh.clone(),
                stateid: *stateid,
            });
        }
        result
    }

    pub(crate) fn close_unregistered(&mut self, fh: &NfsFh, stateid: &stateid4) -> RpcResult<()> {
        let result = self.close_cleanup_slot(fh, stateid, OwnerSlot::User);
        if result.as_ref().is_err_and(|error| {
            !matches!(
                error.status,
                nfsstat4_NFS4ERR_BAD_STATEID | nfsstat4_NFS4ERR_OLD_STATEID
            )
        }) {
            self.deferred_path_closes.push(CloseOp {
                fh: fh.clone(),
                stateid: *stateid,
            });
        }
        result
    }

    // Preserve the original OPEN/setup error and the path owner's successful
    // CLOSE sequence accounting. On failure even a partially closed batch is
    // safe to retain: the drain consumes terminal stateids without replaying OPEN.
    fn close_open_cleanup(&mut self, closes: Vec<CloseOp>, slot: OwnerSlot) {
        let result = match slot {
            OwnerSlot::User => self.close_many(&closes),
            OwnerSlot::Path => self.close_many_path(&closes),
        };
        if result.is_err() {
            self.deferred_path_closes.extend(closes);
        }
    }

    fn close_cleanup_slot(
        &mut self,
        fh: &NfsFh,
        stateid: &stateid4,
        slot: OwnerSlot,
    ) -> RpcResult<()> {
        #[cfg(feature = "test-faults")]
        if let Some(injector) = &self.fault_injector
            && let Err(error) = injector.check(&OpenFaultPoint::BeforePathClose)
        {
            return Err(RpcError::transport(error.to_string()));
        }
        self.close_slot(fh, stateid, slot)?;
        #[cfg(feature = "test-faults")]
        {
            self.confirmed_path_closes += 1;
        }
        Ok(())
    }

    fn close_slot(&mut self, fh: &NfsFh, stateid: &stateid4, slot: OwnerSlot) -> RpcResult<()> {
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
            self.session.path_owner.seqid = self.session.path_owner.seqid.wrapping_add(1);
        }
        Ok(())
    }

    /// CREATE a new object below `dir`. `ftype` is the object type (NF4DIR
    /// for mkdir, NF4LNK for a symlink with `linkdata`). Returns the handle
    /// of the new object.
    fn create(
        &mut self,
        dir: &NfsFh,
        name: &str,
        ftype: nfs_ftype4,
        linkdata: Option<&str>,
    ) -> RpcResult<NfsFh> {
        let mut c = Compound::new();
        c.tag(b"create");
        c.putfh(&dir.as_nfs_fh());
        c.create(name.as_bytes(), ftype, linkdata.map(|s| s.as_bytes()), None);
        c.getfh();
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(NfsFh::from_nfs_fh(res.getfh(3)))
    }

    /// Create a directory `name` below `dir`.
    pub fn mkdir(&mut self, dir: &NfsFh, name: &str) -> RpcResult<NfsFh> {
        self.create(dir, name, nfs_ftype4_NF4DIR, None)
    }

    /// Create a symbolic link `name` below `dir` pointing at `target`.
    pub fn symlink(&mut self, dir: &NfsFh, name: &str, target: &str) -> RpcResult<NfsFh> {
        self.create(dir, name, nfs_ftype4_NF4LNK, Some(target))
    }

    /// Read the target of the symlink at `fh`.
    pub fn readlink(&mut self, fh: &NfsFh) -> RpcResult<Vec<u8>> {
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
    pub fn getattr(&mut self, fh: &NfsFh, attrs: &[u32]) -> RpcResult<Vec<u8>> {
        self.getattr_with_bitmap(fh, attrs).map(|(_, bytes)| bytes)
    }

    pub(crate) fn getattr_with_bitmap(
        &mut self,
        fh: &NfsFh,
        attrs: &[u32],
    ) -> RpcResult<ReturnedAttrs> {
        let mut c = Compound::new();
        c.tag(b"getattr");
        c.putfh(&fh.as_nfs_fh());
        c.getattr(attrs);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(res.getattr_with_bitmap(2))
    }

    /// SETATTR mode and/or size on `fh`.
    pub fn setattr(&mut self, fh: &NfsFh, mode: Option<u32>, size: Option<u64>) -> RpcResult<()> {
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
        fh: &NfsFh,
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
        fh: &NfsFh,
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

    /// Read one page from a directory. Use DirCookie::default() initially,
    /// then pass the returned cookie/verifier together until value is zero.
    /// Entry attributes are encoded according to each entry's attrmask.
    pub fn readdir(
        &mut self,
        dir: &NfsFh,
        cookie: DirCookie,
        attrs: &[u32],
    ) -> RpcResult<ReaddirPage> {
        self.readdir_bounded(dir, cookie, attrs, usize::MAX)
    }

    /// READDIR with a caller-imposed reply budget. The server-negotiated
    /// response limit remains the upper bound; tiny requested limits are
    /// raised to 4 KiB so a valid reply can hold at least one entry.
    pub fn readdir_bounded(
        &mut self,
        dir: &NfsFh,
        cookie: DirCookie,
        attrs: &[u32],
        max_response_bytes: usize,
    ) -> RpcResult<ReaddirPage> {
        let page = self.readdir_wire_bounded(dir, cookie, attrs, max_response_bytes)?;
        self.finish_readdir_page(dir, cookie, page, attrs, max_response_bytes)
    }

    // Dot entries are accepted and filtered, so an empty decoded page is not
    // necessarily EOF. Consume those pages before exposing a logical page.
    fn finish_readdir_page(
        &mut self,
        dir: &NfsFh,
        requested: DirCookie,
        mut page: ReaddirPage,
        attrs: &[u32],
        max_response_bytes: usize,
    ) -> RpcResult<ReaddirPage> {
        if !page.0.is_empty() || page.1.value == 0 {
            return Ok(page);
        }
        let mut seen = std::collections::HashSet::from([requested.value]);
        while page.0.is_empty() && page.1.value != 0 {
            if !seen.insert(page.1.value) {
                return Err(RpcError::transport("READDIR continuation cookie repeated"));
            }
            page = self.readdir_wire_bounded(dir, page.1, attrs, max_response_bytes)?;
            if page.1.value != 0 && seen.contains(&page.1.value) {
                return Err(RpcError::transport("READDIR continuation cookie repeated"));
            }
        }
        Ok(page)
    }

    fn readdir_wire_bounded(
        &mut self,
        dir: &NfsFh,
        cookie: DirCookie,
        attrs: &[u32],
        max_response_bytes: usize,
    ) -> RpcResult<ReaddirPage> {
        let mut c = Compound::new();
        c.tag(b"readdir");
        c.putfh(&dir.as_nfs_fh());
        let (dircount, maxcount) = self.readdir_limits(1);
        let maxcount = maxcount.min(max_response_bytes.max(4096).min(u32::MAX as usize) as u32);
        let dircount = dircount.min(maxcount / 4).max(1);
        c.readdir(cookie.value, &cookie.verifier, dircount, maxcount, attrs);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Self::collect_readdir(res.readdir(2))
    }

    /// Extract the entries and the next cookie from a decoded READDIR reply.
    fn collect_readdir(ok: &READDIR4resok) -> RpcResult<ReaddirPage> {
        let mut out = Vec::new();
        let mut cookie = 0u64;
        let mut e = ok.reply.entries;
        while !e.is_null() {
            // SAFETY: the generated XDR decoder owns this linked-list node; the reply
            // remains alive for the entire traversal.
            let ent = unsafe { &*e };
            let name_len = ent.name.utf8string_len as usize;
            let name = if name_len == 0 {
                Vec::new()
            } else {
                // SAFETY: the live decoded entry owns name_len initialized name bytes; the
                // zero-length case was handled separately.
                unsafe {
                    std::slice::from_raw_parts(ent.name.utf8string_val as *const u8, name_len)
                }
                .to_vec()
            };
            if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
                return Err(RpcError::transport_with_kind(
                    vfsi_core::TransportKind::InvalidReply,
                    "READDIR returned an invalid name component",
                ));
            }
            if name != b"." && name != b".." {
                let attrs_len = ent.attrs.attr_vals.attrlist4_len as usize;
                let attrs = if attrs_len == 0 {
                    Vec::new()
                } else {
                    // SAFETY: the live decoded entry owns attrs_len initialized attribute
                    // bytes; the zero-length case was handled separately.
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
                    attrmask: ent.attrs.attrmask,
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
        Ok((
            out,
            DirCookie {
                value: if ok.reply.eof != 0 { 0 } else { cookie },
                verifier: ok.cookieverf,
            },
        ))
    }

    /// Acquire one anchored child identity and bounded first page per input.
    /// Known successful compound prefixes are retained across sibling failures.
    pub fn readdir_children(
        &mut self,
        ops: &[(NfsFh, Vec<u8>)],
        attrs: &[u32],
    ) -> vfsi_core::VecResult<ChildListing> {
        self.readdir_children_bounded(ops, attrs, usize::MAX)
    }

    pub(crate) fn readdir_children_bounded(
        &mut self,
        ops: &[(NfsFh, Vec<u8>)],
        attrs: &[u32],
        max_page_bytes: usize,
    ) -> vfsi_core::VecResult<ChildListing> {
        let response_bytes = self.read_compound_bytes();
        let results = self.fixed_results::<4, _, _>(
            b"readdir_children",
            ops,
            usize::MAX,
            |_| 0,
            |compound, (parent, name), count| {
                let (dircount, maxcount) =
                    readdir_response_limits(response_bytes, count, max_page_bytes);
                compound.putfh(&parent.as_nfs_fh());
                compound.lookup(name);
                compound.getfh();
                compound.readdir(0, &[0; 8], dircount, maxcount, attrs);
            },
            |reply, local, _| {
                let (entries, cookie) = Self::collect_readdir(reply.readdir(4 + 4 * local))
                    .map_err(|cause| {
                        vfsi_core::OpError::unknown(vfsi_core::error_from_rpc(cause, None), None)
                    })?;
                Ok(ChildListing {
                    fh: NfsFh::from_nfs_fh(reply.getfh(3 + 4 * local)),
                    entries,
                    cookie,
                })
            },
        )?;
        let mut fatal = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .find(|error| uncertain_completion(error.cause()))
            .map(|error| error.cause().clone());
        Ok(results
            .into_iter()
            .map(|result| {
                result.and_then(|page| {
                    if !(page.entries.is_empty() && page.cookie.value != 0) {
                        return Ok(page);
                    }
                    if let Some(cause) = &fatal {
                        return Err(vfsi_core::OpError::failed(cause.clone(), None));
                    }
                    let (entries, cookie) = self
                        .finish_readdir_page(
                            &page.fh,
                            DirCookie::default(),
                            (page.entries, page.cookie),
                            attrs,
                            max_page_bytes,
                        )
                        .map_err(|error| {
                            let cause = vfsi_core::error_from_rpc(error, None);
                            if uncertain_completion(&cause) {
                                fatal = Some(cause.clone());
                                vfsi_core::OpError::unknown(cause, None)
                            } else {
                                vfsi_core::OpError::failed(cause, None)
                            }
                        })?;
                    Ok(ChildListing {
                        fh: page.fh,
                        entries,
                        cookie,
                    })
                })
            })
            .collect())
    }

    /// Return one page per independent retained directory/cookie input.
    /// A failed input does not discard already decoded pages or healthy siblings.
    pub fn readdir_pages(
        &mut self,
        ops: &[(NfsFh, DirCookie)],
        attrs: &[u32],
    ) -> vfsi_core::VecResult<ReaddirPage> {
        self.readdir_pages_bounded(ops, attrs, usize::MAX)
    }

    /// One visible page per directory; filtered-only wire pages are consumed internally.
    pub(crate) fn readdir_pages_bounded(
        &mut self,
        ops: &[(NfsFh, DirCookie)],
        attrs: &[u32],
        max_page_bytes: usize,
    ) -> vfsi_core::VecResult<ReaddirPage> {
        let response_bytes = self.read_compound_bytes();
        let results = self.fixed_results::<2, _, _>(
            b"readdir_pages",
            ops,
            usize::MAX,
            |_| 0,
            |compound, (fh, cookie), count| {
                let (dircount, maxcount) =
                    readdir_response_limits(response_bytes, count, max_page_bytes);
                compound.putfh(&fh.as_nfs_fh());
                compound.readdir(cookie.value, &cookie.verifier, dircount, maxcount, attrs);
            },
            |reply, local, _| {
                Self::collect_readdir(reply.readdir(2 + 2 * local)).map_err(|cause| {
                    vfsi_core::OpError::unknown(vfsi_core::error_from_rpc(cause, None), None)
                })
            },
        )?;
        let mut fatal = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .find(|error| uncertain_completion(error.cause()))
            .map(|error| error.cause().clone());
        Ok(results
            .into_iter()
            .zip(ops)
            .map(|(result, (fh, requested))| {
                result.and_then(|page| {
                    if !(page.0.is_empty() && page.1.value != 0) {
                        return Ok(page);
                    }
                    if let Some(cause) = &fatal {
                        return Err(vfsi_core::OpError::failed(cause.clone(), None));
                    }
                    self.finish_readdir_page(fh, *requested, page, attrs, max_page_bytes)
                        .map_err(|error| {
                            let cause = vfsi_core::error_from_rpc(error, None);
                            if uncertain_completion(&cause) {
                                fatal = Some(cause.clone());
                                vfsi_core::OpError::unknown(cause, None)
                            } else {
                                vfsi_core::OpError::failed(cause, None)
                            }
                        })
                })
            })
            .collect())
    }

    /// REMOVE `name` from directory `dir`.
    pub fn remove(&mut self, dir: &NfsFh, name: &str) -> RpcResult<()> {
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
        srcdir: &NfsFh,
        oldname: &str,
        dstdir: &NfsFh,
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
    pub fn link(&mut self, dir: &NfsFh, src: &NfsFh, newname: &str) -> RpcResult<()> {
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

fn session_mount_root(session: &mut Session) -> RpcResult<NfsFh> {
    let mut c = Compound::new();
    c.tag(b"mount");
    c.putrootfh();
    c.getfh();
    let res = session.compound(&mut c)?;
    session.expect_all_ok(&res)?;
    Ok(NfsFh::from_nfs_fh(res.getfh(2)))
}

/// Build the `openflag4` for an OPEN based on the create mode.
fn make_open_how(create: OpenCreate) -> openflag4 {
    match create {
        OpenCreate::NoCreate => crate::compound::no_create_how(),
        OpenCreate::Exclusive => openflag4 {
            opentype: opentype4_OPEN4_CREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: createhow4 {
                    mode: createmode4_EXCLUSIVE4,
                    createhow4_u: createhow4__bindgen_ty_1 {
                        createverf: crate::session::make_verifier(),
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
#[allow(clippy::undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn)]
mod tests {

    #[test]
    fn mkdir_never_reports_success_without_requested_mode_acknowledgement() {
        let mut set = nfsv41_sys::bitmap4 {
            bitmap4_len: 0,
            map: [0; 3],
        };
        assert!(super::validate_create_mode(None, &set).is_ok());
        let error = super::validate_create_mode(Some(0o700), &set).unwrap_err();
        assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
        set.bitmap4_len = 2;
        set.map[1] = 1 << (nfsv41_sys::FATTR4_MODE % 32);
        assert!(super::validate_create_mode(Some(0o700), &set).is_ok());
    }
    #[test]
    fn file_sync_writes_reject_weaker_durability_acknowledgements() {
        for committed in [stable_how4_UNSTABLE4, stable_how4_DATA_SYNC4] {
            for count in [0, 2, 4] {
                let error = NfsClient::validate_write_completion(count, committed, 4)
                    .expect_err("FILE_SYNC writes cannot acknowledge weaker durability");
                assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
                assert_eq!(
                    error.cause().transport_kind(),
                    Some(vfsi_core::TransportKind::InvalidReply)
                );
            }
        }
        assert_eq!(
            NfsClient::validate_write_completion(2, stable_how4_FILE_SYNC4, 4),
            Ok((2, stable_how4_FILE_SYNC4))
        );
    }

    #[test]
    fn write_completion_validation_keeps_short_counts_and_isolates_malformed_replies() {
        use vfsi_core::FailureState;
        let results = super::completion::decode_fixed::<2, _>(
            5,
            11,
            0,
            |_| 0,
            |index| {
                let (count, committed, requested) = [
                    (4, stable_how4_FILE_SYNC4, 4),
                    (5, stable_how4_FILE_SYNC4, 4),
                    (2, stable_how4_DATA_SYNC4, 4),
                    (4, stable_how4_FILE_SYNC4 + 1, 4),
                    (0, stable_how4_UNSTABLE4, 4),
                ][index];
                NfsClient::validate_write_completion(count, committed, requested)
            },
        )
        .unwrap();
        assert_eq!(results[0], Ok((4, stable_how4_FILE_SYNC4)));
        for index in [1, 2, 3, 4] {
            let error = results[index].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::Unknown);
            assert!(error.cause().is_transport());
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        }
    }

    #[test]
    fn cleanup_error_remapping_preserves_transport_errors_as_unattributed() {
        let transport = super::remap_cleanup_error(
            super::RpcError::transport("cleanup transport failure"),
            &[2, 5],
        );
        assert!(transport.is_transport());
        assert_eq!(transport.op_index, 0);

        let nfs = super::remap_cleanup_error(super::RpcError::op(1, 10001), &[2, 5]);
        assert!(!nfs.is_transport());
        assert_eq!(nfs.op_index, 5);
    }

    #[test]
    fn exclusive_open_attrset_selects_only_verifier_storage_timestamps() {
        // OPEN reports the attributes used for the verifier, not necessarily
        // the write-only *_SET attributes that a subsequent SETATTR uses.
        for (access, modify) in [
            (
                nfsv41_sys::FATTR4_TIME_ACCESS,
                nfsv41_sys::FATTR4_TIME_MODIFY,
            ),
            (
                nfsv41_sys::FATTR4_TIME_ACCESS_SET,
                nfsv41_sys::FATTR4_TIME_MODIFY_SET,
            ),
        ] {
            let attrset = nfsv41_sys::bitmap4 {
                bitmap4_len: 2,
                map: [
                    0,
                    (1 << (access % 32))
                        | (1 << (modify % 32))
                        | (1 << (nfsv41_sys::FATTR4_MODE % 32)),
                    0,
                ],
            };
            assert_eq!(
                super::verifier_storage_attrs(&attrset),
                super::VerifierStorageAttrs {
                    mode: true,
                    atime: true,
                    mtime: true,
                }
            );
        }
    }

    #[test]
    fn exclusive_cleanup_setattr_combines_creation_mode_and_reported_verifier_times() {
        let fh = super::NfsFh::test_from_bytes(vec![1, 2, 3]);
        let update = super::verifier_cleanup_setattr(
            &fh,
            super::VerifierStorageAttrs {
                mode: false,
                atime: true,
                mtime: false,
            },
            Some(0o600),
            (123, 456),
        );
        assert_eq!(update.fh, fh);
        assert_eq!(update.mode, Some(0o600));
        assert_eq!(update.atime, Some((123, 456)));
        assert_eq!(update.mtime, None);
        assert_eq!(update.uid, None);
        assert_eq!(update.gid, None);
        assert_eq!(update.size, None);

        let fallback = super::verifier_cleanup_setattr(
            &fh,
            super::VerifierStorageAttrs {
                mode: true,
                atime: false,
                mtime: false,
            },
            None,
            (123, 456),
        );
        assert_eq!(fallback.mode, Some(0o600));
    }

    #[test]
    fn fixed_bytes_filehandle_layout_and_wire_ownership() {
        use std::collections::HashSet;
        use std::mem::size_of;

        assert_eq!(size_of::<super::NfsFh>(), size_of::<Box<[u8]>>());
        assert_eq!(
            size_of::<Vec<u8>>() - size_of::<super::NfsFh>(),
            size_of::<usize>()
        );

        let mut source = [0, 255, 17, 0, 42];
        let expected = source;
        let wire = nfsv41_sys::nfs_fh4 {
            nfs_fh4_len: source.len() as u32,
            nfs_fh4_val: source.as_mut_ptr().cast(),
        };
        let handle = super::NfsFh::from_nfs_fh(&wire);
        source.fill(99);
        assert_eq!(&*handle.bytes, &expected);
        assert_eq!(handle.len(), expected.len());
        assert!(!handle.is_empty());

        let cloned = handle.clone();
        assert_eq!(cloned, handle);
        assert_ne!(cloned.bytes.as_ptr(), handle.bytes.as_ptr());
        let mut set = HashSet::new();
        set.insert(handle);
        assert!(set.contains(&cloned));
        drop(set);
        let cloned_wire = cloned.as_nfs_fh();
        assert_eq!(cloned_wire.nfs_fh4_len as usize, expected.len());
        let bytes = unsafe {
            std::slice::from_raw_parts(cloned_wire.nfs_fh4_val.cast::<u8>(), expected.len())
        };
        assert_eq!(bytes, expected);

        let empty: [u8; 0] = [];
        let empty_handle = super::NfsFh::from_nfs_fh(&nfsv41_sys::nfs_fh4 {
            nfs_fh4_len: 0,
            nfs_fh4_val: empty.as_ptr() as *mut std::ffi::c_char,
        });
        assert!(empty_handle.is_empty());
        assert_eq!(empty_handle.len(), 0);

        let null_empty_handle = super::NfsFh::from_nfs_fh(&nfsv41_sys::nfs_fh4 {
            nfs_fh4_len: 0,
            nfs_fh4_val: std::ptr::null_mut(),
        });
        assert!(null_empty_handle.is_empty());
        assert_eq!(null_empty_handle.len(), 0);
    }

    #[test]
    fn allocation_path_components_borrow_input_and_preserve_cursor_navigation() {
        let input = b"///a//\xff/.././leaf///";
        let components: Vec<_> = super::path_components(input).collect();
        assert_eq!(
            components,
            [&b"a"[..], &b"\xff"[..], &b".."[..], &b"."[..], &b"leaf"[..]]
        );
        assert_eq!(components[0].as_ptr(), input[3..].as_ptr());
        assert_eq!(components[1].as_ptr(), input[6..].as_ptr());
        let mut cursor = super::CfhCursor::default();
        let mut compound = crate::compound::Compound::new();
        let (_, ops) = cursor.set_parent(&mut compound, b"a/\xff/first").unwrap();
        assert_eq!(ops, 4);
        cursor.descend();
        let (_, ops) = cursor
            .set_current_parent(&mut compound, b"a/\xfe/second")
            .unwrap();
        assert_eq!(ops, 3); // RESTOREFH, LOOKUPP, LOOKUP
        assert_eq!(cursor.saved_dir.as_deref(), Some(&b"a/\xff"[..]));
        let before = compound.op_count();
        let (_, ops) = cursor.set_parent(&mut compound, b"z/deep/third").unwrap();
        assert_eq!(compound.op_count() - before, ops);
        assert_eq!(ops, 4); // Root walk beats climbing the unrelated saved path.
        assert_eq!(cursor.saved_dir.as_deref(), Some(&b"z/deep"[..]));
        cursor.descend();
        let before = compound.op_count();
        let (_, ops) = cursor
            .set_current_parent(&mut compound, b"other/new/fourth")
            .unwrap();
        assert_eq!(compound.op_count() - before, ops);
        assert_eq!(ops, 3);
        assert_eq!(cursor.saved_dir.as_deref(), Some(&b"z/deep"[..]));
    }

    #[test]
    fn fixed_bytes_cursor_preserves_saved_parent_reuse() {
        let mut cursor = super::CfhCursor::default();
        let mut compound = crate::compound::Compound::new();
        let (leaf, first_ops) = cursor.set_parent(&mut compound, b"a/b/first").unwrap();
        assert_eq!(leaf, b"first");
        assert_eq!(first_ops, 4); // PUTROOTFH, LOOKUP a/b, SAVEFH
        assert_eq!(cursor.saved_dir.as_deref(), Some(b"a/b".as_slice()));
        cursor.at_saved = false;
        let (leaf, ops) = cursor.set_parent(&mut compound, b"a/b/second").unwrap();
        assert_eq!(leaf, b"second");
        assert_eq!(ops, 1); // RESTOREFH
        let (_, ops) = cursor.set_parent(&mut compound, b"a/c/third").unwrap();
        assert_eq!(ops, 3); // LOOKUPP, LOOKUP c, SAVEFH
        assert_eq!(cursor.saved_dir.as_deref(), Some(b"a/c".as_slice()));
        let (_, ops) = cursor
            .set_current_parent(&mut compound, b"a/c/fourth")
            .unwrap();
        assert_eq!(ops, 0);
        assert_eq!(cursor.saved_dir.as_deref(), Some(b"a/c".as_slice()));
    }

    #[test]
    fn shared_parent_navigation_preserves_save_restore_and_exact_operation_counts() {
        let parents: &[Option<&[u8]>] =
            &[None, Some(b""), Some(b"a"), Some(b"a/b"), Some(b"z/\xff")];
        for &saved in parents {
            for path in [
                b"leaf".as_slice(),
                b"a/leaf",
                b"a/b/leaf",
                b"a/c/leaf",
                b"z/\xff/leaf",
                b"/a//b/leaf///",
            ] {
                for at_saved in [false, true] {
                    for save in [false, true] {
                        let (dir, expected_leaf) = super::split_path_bytes(path).unwrap();
                        let components: Vec<_> = super::path_components(dir).collect();
                        let walk = match saved {
                            Some(saved) if saved == dir => usize::from(!at_saved),
                            Some(saved) => {
                                let prior: Vec<_> = super::path_components(saved).collect();
                                let common = prior
                                    .iter()
                                    .zip(&components)
                                    .take_while(|(a, b)| a == b)
                                    .count();
                                let relative = prior.len() + components.len() - 2 * common;
                                if relative < 1 + components.len() {
                                    relative + usize::from(!at_saved)
                                } else {
                                    1 + components.len()
                                }
                            }
                            None => 1 + components.len(),
                        };
                        let mut cursor = super::CfhCursor {
                            saved_dir: saved.map(Into::into),
                            at_saved,
                        };
                        let mut compound = crate::compound::Compound::new();
                        let (leaf, count) = if save {
                            cursor.set_parent(&mut compound, path)
                        } else {
                            cursor.set_current_parent(&mut compound, path)
                        }
                        .unwrap();
                        assert_eq!(
                            leaf.as_ptr(),
                            expected_leaf.as_ptr(),
                            "leaf must remain borrowed"
                        );
                        assert_eq!(count, walk + usize::from(save && saved != Some(dir)));
                        assert_eq!(compound.op_count(), count);
                        assert_eq!(
                            cursor.saved_dir.as_deref(),
                            if save { Some(dir) } else { saved }
                        );
                        assert_eq!(cursor.at_saved, save || saved == Some(dir));
                    }
                }
            }
        }
    }

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
    fn read_reply_data_never_exceeds_the_requested_count() {
        assert!(super::NfsClient::validate_read_length(0, 0).is_ok());
        assert!(super::NfsClient::validate_read_length(4, 5).is_ok());
        assert!(super::NfsClient::validate_read_length(5, 5).is_ok());

        let error = super::NfsClient::validate_read_length(6, 5).unwrap_err();
        assert!(error.is_transport());
        assert!(error.to_string().contains("exceeded requested count"));
    }

    #[test]
    fn oversized_path_read_reply_retains_both_temporary_cleanup_targets() {
        let opened = vec![
            (
                super::NfsFh {
                    bytes: Box::from([1u8, 2]),
                },
                nfsv41_sys::stateid4 {
                    seqid: 3,
                    other: [4; 12],
                },
            ),
            (
                super::NfsFh {
                    bytes: Box::from([5u8]),
                },
                nfsv41_sys::stateid4 {
                    seqid: 6,
                    other: [7; 12],
                },
            ),
        ];
        let error = super::NfsClient::validate_read_length(6, 5).unwrap_err();
        let closed: Vec<_> = super::latest_path_open_closes(&opened)
            .iter()
            .map(|op| (op.fh.clone(), op.stateid.seqid, op.stateid.other))
            .collect();
        assert!(error.is_transport());
        assert_eq!(
            closed,
            vec![
                (opened[0].0.clone(), 3, [4; 12]),
                (opened[1].0.clone(), 6, [7; 12]),
            ]
        );
    }

    #[test]
    fn directory_page_eof_suppresses_continuation_without_rewriting_entry_cookies() {
        let mut name = b"entry".to_vec();
        let mut entry: nfsv41_sys::entry4 = unsafe { std::mem::zeroed() };
        entry.cookie = 42;
        entry.name.utf8string_len = name.len() as _;
        entry.name.utf8string_val = name.as_mut_ptr().cast();
        let mut reply: nfsv41_sys::READDIR4resok = unsafe { std::mem::zeroed() };
        reply.reply.entries = &mut entry;
        reply.cookieverf = [5; 8];
        reply.reply.eof = 1;
        let (entries, next) = super::NfsClient::collect_readdir(&reply).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cookie, 42);
        assert_eq!(next.value, 0);
        assert_eq!(next.verifier, [5; 8]);
        reply.reply.eof = 0;
        assert_eq!(
            super::NfsClient::collect_readdir(&reply).unwrap().1.value,
            42
        );
        reply.reply.entries = std::ptr::null_mut();
        assert!(
            super::NfsClient::collect_readdir(&reply)
                .unwrap_err()
                .is_transport()
        );
    }

    #[test]
    fn readdir_rejects_invalid_name_components_as_invalid_replies() {
        for invalid in [&b""[..], b"/sibling", b"../sibling", b"file\0name"] {
            let mut name = invalid.to_vec();
            let mut entry: nfsv41_sys::entry4 = unsafe { std::mem::zeroed() };
            entry.cookie = 1;
            entry.name.utf8string_len = name.len() as _;
            entry.name.utf8string_val = name.as_mut_ptr().cast();
            let mut reply: nfsv41_sys::READDIR4resok = unsafe { std::mem::zeroed() };
            reply.reply.entries = &mut entry;
            reply.reply.eof = 1;

            let error = super::NfsClient::collect_readdir(&reply).unwrap_err();
            assert_eq!(
                error.transport_kind,
                Some(vfsi_core::TransportKind::InvalidReply),
                "invalid name {invalid:?}"
            );
        }
    }

    #[test]
    fn readdir_preserves_non_utf8_name_components() {
        let mut name = vec![0xff];
        let mut entry: nfsv41_sys::entry4 = unsafe { std::mem::zeroed() };
        entry.cookie = 1;
        entry.name.utf8string_len = name.len() as _;
        entry.name.utf8string_val = name.as_mut_ptr().cast();
        let mut reply: nfsv41_sys::READDIR4resok = unsafe { std::mem::zeroed() };
        reply.reply.entries = &mut entry;
        reply.reply.eof = 1;

        let (entries, next) = super::NfsClient::collect_readdir(&reply).unwrap();
        assert_eq!(entries[0].name, name);
        assert_eq!(next.value, 0);
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

    #[test]
    fn failed_path_batch_preparation_closes_every_open_and_keeps_primary_error() {
        let opened = vec![
            (
                NfsFh {
                    bytes: Box::from([1u8, 2]),
                },
                nfsv41_sys::stateid4 {
                    seqid: 3,
                    other: [4; 12],
                },
            ),
            (
                NfsFh {
                    bytes: Box::from([5u8]),
                },
                nfsv41_sys::stateid4 {
                    seqid: 6,
                    other: [7; 12],
                },
            ),
        ];
        let primary_error = "SETATTR failed";
        let mut closed = Vec::new();

        let mut client = ();
        let result = super::prepare_path_opens::<_, (), _>(
            &opened,
            &mut client,
            |_| Err(primary_error),
            |_, ops| {
                closed.extend(
                    ops.iter()
                        .map(|op| (op.fh.clone(), op.stateid.seqid, op.stateid.other)),
                );
            },
        );

        assert_eq!(result, Err(primary_error));
        assert_eq!(
            closed,
            vec![
                (opened[0].0.clone(), 3, [4; 12]),
                (opened[1].0.clone(), 6, [7; 12]),
            ]
        );
    }
}

#[cfg(test)]
mod sequence_tests;

#[cfg(all(test, feature = "test-faults"))]
mod readlink_results_tests;

#[cfg(all(test, feature = "test-faults"))]
mod link_results_tests;

#[cfg(all(test, feature = "test-faults"))]
mod setattr_results_tests;

#[cfg(all(test, feature = "test-faults"))]
mod rename_results_tests;

#[cfg(all(test, feature = "test-faults"))]
mod copy_results_tests;

#[cfg(all(test, feature = "test-faults"))]
mod remove_results_tests;

#[cfg(all(test, feature = "test-faults"))]
mod readdir_results_tests;

#[cfg(all(test, feature = "test-faults"))]
mod uncached_reply_tests;
