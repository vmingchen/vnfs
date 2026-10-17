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
use crate::error::{RpcError, RpcResult};
use crate::path::split_path_bytes;
use crate::planner::{
    AdaptiveCompoundLimits, ExecutionMap, FailureCause, RecoveryAction, RequestSafety,
    recovery_action, resource_rejected_before_mutation,
};
use crate::session::Session;
#[path = "client_io_progress.rs"]
mod io_progress;
use io_progress::{complete_read_suffix, complete_write_suffix};
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};

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

/// Compatibility name for the same opaque [`NfsFh`] object reference.
#[deprecated(note = "Use NfsFh instead")]
pub use NfsFh as FileHandle;

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
    pub fh: NfsFh,
    pub entries: Vec<DirEntry>,
    pub cookie: u64,
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

fn read_error_with_cleanup<T, C>(
    opened: &[(NfsFh, stateid4)],
    close_in_compound: bool,
    client: &mut C,
    error: RpcError,
    close: impl FnOnce(&mut C, &[CloseOp]),
) -> RpcResult<T> {
    if !close_in_compound && !opened.is_empty() {
        // Only these decoder-owned OPENs share the temporary path owner.
        // Generic/public OPEN cleanup must retain independent instances.
        close(client, &latest_path_open_closes(opened));
    }
    Err(error)
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

/// Per-op results of a merged path compound. `counts`/`committed` are `None`
/// for ops that never executed (the compound aborted at `failed`).
pub struct PathWriteOutcome {
    pub counts: Vec<Option<u32>>,
    pub committed: Vec<Option<u32>>,
    /// Open stateids (with their filehandles) created by the compound; used
    /// by the separate-close form and for best-effort cleanup on failure.
    pub opened: Vec<(NfsFh, stateid4)>,
    /// First failing caller-relative op index + NFS status, if any.
    pub failed: Option<(usize, u32)>,
    /// The trailing in-compound CLOSE failed (special stateid unsupported).
    pub close_failed: Option<u32>,
}

/// Per-op results of a merged path read compound.
pub struct PathReadOutcome {
    pub data: Vec<Option<Vec<u8>>>,
    pub eof: Vec<Option<bool>>,
    pub opened: Vec<(NfsFh, stateid4)>,
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
    pub opened: Vec<Option<(NfsFh, stateid4)>>,
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
        if let Some(saved) = self.saved_dir.as_deref() {
            let saved_comps = path_components(saved);
            let target_comps = path_components(&dir);
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
                c.savefh();
                ops += 1;
                self.saved_dir = Some(dir.into_boxed_slice());
                self.at_saved = true;
                return Some((leaf, ops));
            }
        }
        // Resolve the parent directory from the export root.
        let mut ops = 1; // PUTROOTFH
        c.putrootfh();
        for comp in path_components(&dir) {
            c.lookup(comp);
            ops += 1;
        }
        c.savefh();
        ops += 1;
        self.saved_dir = Some(dir.into_boxed_slice());
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
        if let Some(saved) = self.saved_dir.as_deref() {
            let saved_comps = path_components(saved);
            let target_comps = path_components(&dir);
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
                return Some((leaf, ops));
            }
        }
        let mut ops = 1; // PUTROOTFH
        c.putrootfh();
        for comp in path_components(&dir) {
            c.lookup(comp);
            ops += 1;
        }
        self.at_saved = false; // current fh differs from the saved one
        Some((leaf, ops))
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

#[path = "client_path_open_cleanup.rs"]
mod path_open_cleanup;
pub(crate) use path_open_cleanup::latest_path_open_closes;

#[cfg(all(test, feature = "test-faults"))]
#[path = "client_session_recovery_tests.rs"]
mod session_recovery_tests;

impl NfsClient {
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

    fn close_path_or_defer(&mut self, closes: Vec<CloseOp>) {
        if closes.is_empty() {
            return;
        }
        if self.close_unregistered_many(&closes).is_err() {
            self.deferred_path_closes.extend(closes);
        }
    }

    fn drain_deferred_path_closes(&mut self) -> RpcResult<()> {
        let mut remaining = std::mem::take(&mut self.deferred_path_closes);
        while !remaining.is_empty() {
            let close = remaining.remove(0);
            match self.close_unregistered(&close.fh, &close.stateid) {
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
            #[cfg(feature = "test-faults")]
            short_merged_read_once: None,
            #[cfg(feature = "test-faults")]
            short_merged_write_once: None,
            #[cfg(feature = "test-faults")]
            fail_merged_write_once: None,
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
                        make_open_how(open.create),
                        &leaf,
                    );
                }
                *target.opens_in_chunk += 1;
                target.map.note_ops(1);
                if !target.close_in_compound {
                    target.compound.getfh();
                    target.map.note_ops(1);
                }
                *target.opened_path = Some(Box::from(path.as_slice()));
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

    /// Tolerantly LOOKUP each `(parent, name)` and return the child handle and
    /// FATTR4_TYPE (`[PUTFH, LOOKUP, GETFH, GETATTR]` per element). Failed
    /// LOOKUPs are reported per path; only transport / compound-level
    /// failures abort the whole call.
    pub fn lookup_getattr_many(
        &mut self,
        ops: &[(NfsFh, Vec<u8>)],
    ) -> RpcResult<Vec<Result<(NfsFh, u32), u32>>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Option<Result<(NfsFh, u32), u32>>> =
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

            for &(caller, start, _) in map.completed_ranges(&report) {
                let fh = NfsFh::from_nfs_fh(res.getfh(start + 2));
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

    fn validate_read_length(actual: usize, requested: usize) -> RpcResult<()> {
        if actual > requested {
            Err(RpcError::transport(
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
                        Self::validate_read_length(len, ops[wire_index].count as usize)
                            .map_err(|error| error.with_op_index(local_index))?;
                        let data = res.read_data(2 + 2 * i);
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
    pub fn remove_many(&mut self, dir: &NfsFh, names: &[Vec<u8>]) -> RpcResult<()> {
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
            self.session.path_owner.seqid += 1;
        }
        if create == OpenCreate::Exclusive {
            let attrs = verifier_storage_attrs(&open_result.attrset);
            if post_create_mode.is_some() || attrs.mode || attrs.atime || attrs.mtime {
                let timestamp = current_timestamp();
                let update = verifier_cleanup_setattr(&fh, attrs, post_create_mode, timestamp);
                if let Err(error) = self.setattr_many(&[update]) {
                    let close = [CloseOp {
                        fh: fh.clone(),
                        stateid,
                    }];
                    let _ = match slot {
                        OwnerSlot::User => self.close_many(&close),
                        OwnerSlot::Path => self.close_many_path(&close),
                    };
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
    fn cleanup_exclusive_path_opens(
        &mut self,
        opened: &[Option<(NfsFh, stateid4)>],
        verifier_attrs: &[Option<VerifierStorageAttrs>],
        ops: &[PathOpenOp],
        indices: &[usize],
    ) -> RpcResult<()> {
        let timestamp = current_timestamp();
        let mut cleanup_indices = Vec::new();
        let mut cleanup = Vec::new();
        for &index in indices {
            if ops[index].create != OpenCreate::Exclusive {
                continue;
            }
            let Some((fh, _)) = opened[index].as_ref() else {
                continue;
            };
            let attrs = verifier_attrs[index].unwrap_or_default();
            let mode = ops[index].mode.map(|mode| mode & 0o7777);
            if mode.is_some() || attrs.mode || attrs.atime || attrs.mtime {
                cleanup_indices.push(index);
                cleanup.push(verifier_cleanup_setattr(fh, attrs, mode, timestamp));
            }
        }
        self.setattr_many(&cleanup)
            .map_err(|error| remap_cleanup_error(error, &cleanup_indices))
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
                    client.session.path_owner.seqid = base + opened.len() as u32;
                }
                let closes: Vec<CloseOp> = opened
                    .iter()
                    .map(|(fh, stateid, _)| CloseOp {
                        fh: fh.clone(),
                        stateid: *stateid,
                    })
                    .collect();
                let _ = match slot {
                    OwnerSlot::User => client.close_many(&closes),
                    OwnerSlot::Path => client.close_many_path(&closes),
                };
            },
        )?;
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid = base + n as u32;
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
            if let Err(error) = self.setattr_many(&cleanup) {
                let error = remap_cleanup_error(error, &cleanup_indices);
                let closes: Vec<CloseOp> = out
                    .iter()
                    .map(|(fh, stateid, _)| CloseOp {
                        fh: fh.clone(),
                        stateid: *stateid,
                    })
                    .collect();
                let _ = match slot {
                    OwnerSlot::User => self.close_many(&closes),
                    OwnerSlot::Path => self.close_many_path(&closes),
                };
                return Err(error);
            }
        }
        Ok(out
            .into_iter()
            .map(|(fh, stateid, _)| (fh, stateid))
            .collect())
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

    /// Cleanup confirmed public OPENs that were not transferred to descriptors.
    /// Preserve cleanup fault hooks independently of the chosen OPEN owner.
    pub(crate) fn close_unregistered_many(&mut self, ops: &[CloseOp]) -> RpcResult<()> {
        self.close_many_cleanup_slot(ops, OwnerSlot::User)
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
    fn with_path_io_repair_target<T>(
        &mut self,
        file: &FileRef,
        stateid: Option<&stateid4>,
        access: u32,
        caller: usize,
        repair: impl FnOnce(&mut Self, &NfsFh, &stateid4) -> RpcResult<T>,
    ) -> RpcResult<T> {
        match file {
            FileRef::Handle(fh) => repair(self, fh, stateid.unwrap_or(&SPECIAL_STATEID)),
            FileRef::Path(path) => {
                let (parent, name) = split_path_bytes(path)
                    .map_err(|_| RpcError::op(caller, nfsstat4_NFS4ERR_INVAL))?;
                let parent = self
                    .resolve(&parent)
                    .map_err(|error| error.with_op_index(caller))?;
                let (fh, stateid) = self
                    .open(&parent, &name, access, OpenCreate::NoCreate)
                    .map_err(|error| error.with_op_index(caller))?;
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
    ) -> RpcResult<u32> {
        let op = &ops[caller];
        let offset = checked_offset(op.offset, part, caller)?;
        self.with_path_io_repair_target(
            &op.file,
            op.stateid.as_ref(),
            OPEN4_SHARE_ACCESS_WRITE,
            caller,
            |client, fh, stateid| {
                // Known same-file conflicts are split by WriteWaveAccess.
                // A hard-link alias can only be discovered on this slow path.
                // Never repair an earlier gap through a later accepted write.
                let end = checked_offset(offset, data.len(), caller)?;
                for &(index, _, _) in later {
                    if index <= caller {
                        continue;
                    }
                    let next = &ops[index];
                    let overlaps = next.truncate
                        || (next.offset < end
                            && offset < checked_offset(next.offset, next.data.len(), index)?);
                    if !overlaps {
                        continue;
                    }
                    let alias = match &next.file {
                        FileRef::Handle(handle) => handle == fh,
                        FileRef::Path(path) => {
                            client
                                .resolve(path)
                                .map_err(|error| error.with_op_index(caller))?
                                == *fh
                        }
                    };
                    if alias {
                        return Err(RpcError::op(caller, nfsstat4_NFS4ERR_IO));
                    }
                }
                complete_write_suffix(client, caller, offset, data, |client, offset, data| {
                    client.write(fh, stateid, offset, data)
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
    /// (Ganesha resolves it to the current open); otherwise the open
    /// stateids/filehandles are returned so the caller can CLOSE in a
    /// follow-up compound (the portable fallback).
    ///
    /// On a mid-compound failure, ops before the failing op are reported in
    /// `counts`/`committed` and `failed` carries the caller-relative index
    /// and NFS status. Path references may be resolved again across compound
    /// boundaries or short-I/O repair; use Handle references when object identity
    /// must remain pinned across concurrent namespace changes.
    pub fn writev_path_compound(
        &mut self,
        ops: &[PathWriteOp<'_>],
        close_in_compound: bool,
    ) -> RpcResult<PathWriteOutcome> {
        let n = ops.len();
        let mut counts: Vec<Option<u32>> = vec![None; n];
        let mut committed: Vec<Option<u32>> = vec![None; n];
        let mut opened: Vec<(NfsFh, stateid4)> = Vec::new();
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
            if !close_in_compound {
                res.for_each_open_result(map.executed_ranges(&report), |fh, stateid| {
                    opened.push((NfsFh::from_nfs_fh(fh), stateid));
                });
            }
            // Complete only missing wire suffixes before later compounds.
            // A complete reply retains the original batching and adds no RPC.
            let mut write_index = 0;
            for (caller, s, e) in map.completed_ranges(&report) {
                for j in *s..(*e).min(res.nops()) {
                    let ro = res.op(j);
                    if ro.resop == nfs_opnum4_NFS4_OP_WRITE {
                        let ok = res.write(j);
                        #[cfg(feature = "test-faults")]
                        if injected_short_write {
                            self.short_merged_write_once = None;
                        }
                        let (part, requested) = *expected_writes[..expected_write_count]
                            .get(write_index)
                            .ok_or_else(|| RpcError::transport("extra merged WRITE reply"))?;
                        write_index += 1;
                        let received = ok.count as usize;
                        let result = (|| {
                            if received > requested || (received == 0 && requested != 0) {
                                return Err(RpcError::op(*caller, nfsstat4_NFS4ERR_IO));
                            }
                            let mut stable = ok.committed;
                            if received < requested {
                                stable = stable.min(self.repair_path_write(
                                    ops,
                                    &map.ranges[..report.completed.len()
                                        + usize::from(report.failure.is_some())],
                                    *caller,
                                    part + received,
                                    &ops[*caller].data[part + received..part + requested],
                                )?);
                            }
                            let count = counts[*caller].get_or_insert(0);
                            *count = count.saturating_add(requested as u32);
                            committed[*caller] = Some(
                                committed[*caller]
                                    .unwrap_or(stable_how4_FILE_SYNC4)
                                    .min(stable),
                            );
                            Ok(())
                        })();
                        if let Err(error) = result {
                            return read_error_with_cleanup(
                                &opened,
                                close_in_compound,
                                self,
                                error,
                                |client, closes| {
                                    client.close_path_or_defer(
                                        closes
                                            .iter()
                                            .map(|op| CloseOp {
                                                fh: op.fh.clone(),
                                                stateid: op.stateid,
                                            })
                                            .collect(),
                                    )
                                },
                            );
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
    /// no creation. Path references may be resolved again across compound
    /// boundaries or short-I/O repair; Handle references keep the opened object.
    pub fn readv_path_compound(
        &mut self,
        ops: &[PathReadOp],
        close_in_compound: bool,
    ) -> RpcResult<PathReadOutcome> {
        let n = ops.len();
        let mut data: Vec<Option<Vec<u8>>> = vec![None; n];
        let mut eof: Vec<Option<bool>> = vec![None; n];
        let mut opened: Vec<(NfsFh, stateid4)> = Vec::new();
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
            let mut opened_path: Option<Box<[u8]>> = None;
            let mut fh_at_opened = false;
            let mut opens_in_chunk = 0usize;
            let base_seq = self.session.path_owner.seqid;
            let mut payload = 0usize;
            let mut expected_read_lengths = [0; MAX_COMPOUND_OPS];
            let mut expected_read_count = 0usize;

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
            if !close_in_compound {
                res.for_each_open_result(map.executed_ranges(&report), |fh, stateid| {
                    opened.push((NfsFh::from_nfs_fh(fh), stateid));
                });
            }
            let mut read_index = 0usize;
            for (caller, s, e) in map.completed_ranges(&report) {
                for j in *s..(*e).min(res.nops()) {
                    let ro = res.op(j);
                    if ro.resop == nfs_opnum4_NFS4_OP_READ {
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
                                return read_error_with_cleanup(
                                    &opened,
                                    close_in_compound,
                                    self,
                                    error,
                                    |client, closes| {
                                        client.close_path_or_defer(
                                            closes
                                                .iter()
                                                .map(|op| CloseOp {
                                                    fh: op.fh.clone(),
                                                    stateid: op.stateid,
                                                })
                                                .collect(),
                                        )
                                    },
                                );
                            }
                        };
                        read_index += 1;
                        if let Err(error) = Self::validate_read_length(len, requested as usize) {
                            return read_error_with_cleanup(
                                &opened,
                                close_in_compound,
                                self,
                                error.with_op_index(*caller),
                                |client, closes| {
                                    client.close_path_or_defer(
                                        closes
                                            .iter()
                                            .map(|op| CloseOp {
                                                fh: op.fh.clone(),
                                                stateid: op.stateid,
                                            })
                                            .collect(),
                                    )
                                },
                            );
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
                            let error = RpcError::op(*caller, nfsstat4_NFS4ERR_IO);
                            return read_error_with_cleanup(
                                &opened,
                                close_in_compound,
                                self,
                                error,
                                |client, closes| {
                                    client.close_path_or_defer(
                                        closes
                                            .iter()
                                            .map(|op| CloseOp {
                                                fh: op.fh.clone(),
                                                stateid: op.stateid,
                                            })
                                            .collect(),
                                    )
                                },
                            );
                        }
                        if len < requested as usize && !end_of_file {
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
                                    return read_error_with_cleanup(
                                        &opened,
                                        close_in_compound,
                                        self,
                                        error,
                                        |client, closes| {
                                            client.close_path_or_defer(
                                                closes
                                                    .iter()
                                                    .map(|op| CloseOp {
                                                        fh: op.fh.clone(),
                                                        stateid: op.stateid,
                                                    })
                                                    .collect(),
                                            )
                                        },
                                    );
                                }
                            }
                        }
                        eof[*caller] = Some(end_of_file);
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
        self.setattr_path_compound_inner(ops, None)
    }

    /// Internal variant that inserts an NVERIFY symlink guard before SETATTR
    /// for selected operations. A matching symlink stops that operation before
    /// mutation, allowing the caller to resolve and update the target safely.
    pub(crate) fn setattr_path_compound_guarded(
        &mut self,
        ops: &[PathSetattrOp],
        symlink_guards: &[bool],
    ) -> RpcResult<PathSetattrOutcome> {
        if ops.len() != symlink_guards.len() {
            return Err(RpcError::transport("invalid SETATTR symlink-guard count"));
        }
        self.setattr_path_compound_inner(ops, Some(symlink_guards))
    }

    fn setattr_path_compound_inner(
        &mut self,
        ops: &[PathSetattrOp],
        symlink_guards: Option<&[bool]>,
    ) -> RpcResult<PathSetattrOutcome> {
        let n = ops.len();
        let mut types: Vec<Option<u32>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 5; // RESTOREFH + LOOKUP + [GETATTR or NVERIFY] + SETATTR + margin
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
                if symlink_guards.is_some_and(|guards| guards[global]) {
                    c.nverify_type(nfs_ftype4_NF4LNK);
                    map.note_ops(1);
                } else if op.check_type {
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

    /// Batched path-based OPENs in one compound per chunk, returning the
    /// opened (filehandle, stateid) pairs (the caller keeps them open).
    pub fn openv_path_compound(&mut self, ops: &[PathOpenOp]) -> RpcResult<PathOpenOutcome> {
        let n = ops.len();
        self.drain_deferred_path_closes()?;
        let public_owner_names: Vec<Box<[u8]>> =
            (0..n).map(|_| self.next_public_open_owner_name()).collect();
        let mut opened: Vec<Option<(NfsFh, stateid4)>> = vec![None; n];
        let mut verifier_attrs: Vec<Option<VerifierStorageAttrs>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 6; // RESTOREFH + OPEN + GETFH + margin
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
                        make_open_how(create),
                        &leaf,
                    ),
                }
                map.note_ops(1);
                c.getfh();
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
                            let open_result = res.open(j);
                            let stateid = open_result.stateid;
                            let fh = res.getfh(j + 1);
                            opened[*c] = Some((NfsFh::from_nfs_fh(fh), stateid));
                            if ops[*c].create == OpenCreate::Exclusive {
                                verifier_attrs[*c] =
                                    Some(verifier_storage_attrs(&open_result.attrset));
                            }
                        }
                    }
                }
                let completed: Vec<usize> = map
                    .ranges
                    .iter()
                    .filter_map(|(index, _, _)| (*index < caller).then_some(*index))
                    .collect();
                if let Err(error) =
                    self.cleanup_exclusive_path_opens(&opened, &verifier_attrs, ops, &completed)
                {
                    let closes: Vec<CloseOp> = opened
                        .iter_mut()
                        .filter_map(Option::take)
                        .map(|(fh, stateid)| CloseOp { fh, stateid })
                        .collect();
                    self.close_path_or_defer(closes);
                    return Err(error);
                }
                break;
            }
            let mut completed = Vec::new();
            for (caller, s, e) in &map.ranges {
                for j in *s..*e {
                    if res.op(j).resop == nfs_opnum4_NFS4_OP_OPEN {
                        let open_result = res.open(j);
                        let stateid = open_result.stateid;
                        let fh = res.getfh(j + 1);
                        opened[*caller] = Some((NfsFh::from_nfs_fh(fh), stateid));
                        if ops[*caller].create == OpenCreate::Exclusive {
                            verifier_attrs[*caller] =
                                Some(verifier_storage_attrs(&open_result.attrset));
                        }
                        completed.push(*caller);
                    }
                }
            }
            if let Err(error) =
                self.cleanup_exclusive_path_opens(&opened, &verifier_attrs, ops, &completed)
            {
                let closes: Vec<CloseOp> = opened
                    .iter_mut()
                    .filter_map(Option::take)
                    .map(|(fh, stateid)| CloseOp { fh, stateid })
                    .collect();
                self.close_path_or_defer(closes);
                return Err(error);
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
    pub fn close(&mut self, fh: &NfsFh, stateid: &stateid4) -> RpcResult<()> {
        self.close_slot(fh, stateid, OwnerSlot::User)
    }

    /// Like [`close`](Self::close) but using the path-op open owner.
    pub fn close_path(&mut self, fh: &NfsFh, stateid: &stateid4) -> RpcResult<()> {
        self.close_cleanup_slot(fh, stateid, OwnerSlot::Path)
    }

    pub(crate) fn close_unregistered(&mut self, fh: &NfsFh, stateid: &stateid4) -> RpcResult<()> {
        self.close_cleanup_slot(fh, stateid, OwnerSlot::User)
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
            self.session.path_owner.seqid += 1;
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
        c.create(name.as_bytes(), ftype, linkdata.map(|s| s.as_bytes()));
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
        let mut c = Compound::new();
        c.tag(b"getattr");
        c.putfh(&fh.as_nfs_fh());
        c.getattr(attrs);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(res.getattr_bytes(2))
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

    /// READDIR `dir` starting at `cookie`; returns entries with names, next
    /// cookies, and requested attributes. Skips "." and "..".
    /// READDIR `dir` from `cookie`, requesting the given FATTR4 attributes
    /// for each entry.
    pub fn readdir(&mut self, dir: &NfsFh, cookie: u64, attrs: &[u32]) -> RpcResult<Vec<DirEntry>> {
        self.readdir_bounded(dir, cookie, attrs, usize::MAX)
    }

    /// READDIR with a caller-imposed reply budget. The server-negotiated
    /// response limit remains the upper bound; tiny requested limits are
    /// raised to 4 KiB so a valid reply can hold at least one entry.
    pub fn readdir_bounded(
        &mut self,
        dir: &NfsFh,
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
        ops: &[(NfsFh, Vec<u8>)],
        attrs: &[u32],
    ) -> RpcResult<Vec<ChildListing>> {
        self.readdir_children_bounded(ops, attrs, usize::MAX)
    }

    pub(crate) fn readdir_children_bounded(
        &mut self,
        ops: &[(NfsFh, Vec<u8>)],
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
                    fh: NfsFh::from_nfs_fh(fh),
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
        ops: &[(NfsFh, u64)],
        attrs: &[u32],
    ) -> RpcResult<Vec<(Vec<DirEntry>, u64)>> {
        self.readdir_pages_bounded(ops, attrs, usize::MAX)
    }

    /// One bounded wire page per directory; negotiated response limits still apply.
    pub(crate) fn readdir_pages_bounded(
        &mut self,
        ops: &[(NfsFh, u64)],
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
    fn oversized_path_read_reply_closes_open_stateids_before_returning_error() {
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
        let mut closed = Vec::new();
        let mut client = ();
        let result: super::RpcResult<()> =
            super::read_error_with_cleanup(&opened, false, &mut client, error, |_, ops| {
                closed.extend(
                    ops.iter()
                        .map(|op| (op.fh.clone(), op.stateid.seqid, op.stateid.other)),
                );
            });

        let error = result.unwrap_err();
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
        assert_eq!(next, 0);
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
