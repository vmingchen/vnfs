//! Unstable implementation contracts shared by VFSI backend crates.
//!
//! Nothing in this module is part of the application-facing API. Backends
//! use these types for shared request storage and to retain ordered per-request
//! results until a strict vector API converts them into `Result<Vec<_>, _>`.

pub use crate::write::WriteRequest;

use crate::{VfError, VfResult};

/// Reject malformed open flags for the entire batch before routing or I/O.
/// The native flag translator remains the single source of flag semantics.
pub fn validate_open_requests(requests: &[crate::OpenOp]) -> VfResult<()> {
    for (index, request) in requests.iter().enumerate() {
        crate::open_flags_to_libc(request.flags()).map_err(|error| {
            error
                .with_index(index)
                .with_context("vopen", request.path())
        })?;
    }
    Ok(())
}

/// Validate update values before any I/O and normalize access/modify timestamps.
/// The returned values let backend conversion reuse the checked timestamps.
pub fn validate_setattrs<P>(op: &crate::SetAttrsOp<P>) -> VfResult<[Option<(i64, u32)>; 2]> {
    if op.requested_uid() == Some(u32::MAX) || op.requested_gid() == Some(u32::MAX) {
        return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
    }
    Ok([
        op.requested_accessed().map(system_time_parts).transpose()?,
        op.requested_modified().map(system_time_parts).transpose()?,
    ])
}

fn system_time_parts(time: std::time::SystemTime) -> VfResult<(i64, u32)> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => Ok((
            i64::try_from(duration.as_secs())
                .map_err(|_| VfError::errno(0, libc::EOVERFLOW as u32))?,
            duration.subsec_nanos(),
        )),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs())
                .map_err(|_| VfError::errno(0, libc::EOVERFLOW as u32))?;
            if duration.subsec_nanos() == 0 {
                Ok((-seconds, 0))
            } else {
                let seconds = seconds
                    .checked_add(1)
                    .ok_or_else(|| VfError::errno(0, libc::EOVERFLOW as u32))?;
                Ok((-seconds, 1_000_000_000 - duration.subsec_nanos()))
            }
        }
    }
}
/// Legacy strict collection and cleanup for independent native results.
/// Canonical execution hooks return VecResult; this adapter deliberately
/// selects one error after execution and does not restore prefix semantics.
///
/// Entry `n` always corresponds to request `n`. The vector may be shorter
/// than `requested` when an ordered protocol stopped after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManyResults<T> {
    requested: usize,
    results: Vec<Result<T, VfError>>,
}

impl<T> ManyResults<T> {
    /// Construct ordered backend results.
    ///
    /// Backends may report a contiguous prefix. Invalid cardinality is kept
    /// so strict collection can report a structured contract error instead
    /// of panicking inside an application process.
    pub fn new(requested: usize, results: Vec<Result<T, VfError>>) -> Self {
        Self { requested, results }
    }

    pub fn all_success(values: Vec<T>) -> Self {
        let requested = values.len();
        Self::new(requested, values.into_iter().map(Ok).collect())
    }

    pub fn requested_len(&self) -> usize {
        self.requested
    }

    pub fn reported_len(&self) -> usize {
        self.results.len()
    }

    pub fn is_complete(&self) -> bool {
        self.results.len() == self.requested
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Result<T, VfError>> {
        self.results.iter()
    }

    pub fn into_results(self) -> Vec<Result<T, VfError>> {
        self.results
    }

    /// Apply the strict public policy: return every value or the first error.
    ///
    /// The vector position is authoritative and is attached to the returned
    /// error, regardless of any backend-local index it previously carried.
    pub fn try_collect(self) -> VfResult<Vec<T>> {
        let requested = self.requested;
        let reported = self.results.len();
        if reported > requested {
            return Err(result_count_error(reported, requested));
        }
        let mut values = Vec::with_capacity(reported);
        for (index, result) in self.results.into_iter().enumerate() {
            values.push(result.map_err(|error| error.with_index(index))?);
        }
        if values.len() != requested {
            return Err(result_count_error(values.len(), requested));
        }
        Ok(values)
    }

    /// Apply the strict public policy while cleaning every successful value
    /// if any request failed or the backend returned a short result set.
    ///
    /// Cleanup is best-effort and deliberately cannot replace the primary
    /// operation error. This is used by handle-producing vector operations,
    /// for which simply dropping `T` may not release a remote resource.
    pub fn try_collect_with_cleanup(
        self,
        expected: usize,
        mut cleanup: impl FnMut(usize, &T) -> VfResult<()>,
    ) -> VfResult<Vec<T>> {
        let declared = self.requested;
        let reported = self.results.len();
        let mut values = Vec::with_capacity(expected.min(reported));
        let mut first_error = None;

        for (index, result) in self.results.into_iter().enumerate() {
            match result {
                Ok(value) => values.push((index, value)),
                Err(error) if first_error.is_none() => {
                    first_error = Some(error.with_index(index));
                }
                Err(_) => {}
            }
        }

        let primary = (declared != expected)
            .then(|| declared_count_error(declared, expected))
            .or_else(|| (reported > expected).then(|| result_count_error(reported, expected)))
            .or(first_error)
            .or_else(|| (reported != expected).then(|| result_count_error(reported, expected)));

        if let Some(error) = primary {
            for (index, value) in &values {
                let _ = cleanup(*index, value);
            }
            return Err(error);
        }
        Ok(values.into_iter().map(|(_, value)| value).collect())
    }
}

fn declared_count_error(declared: usize, expected: usize) -> VfError {
    VfError::invalid_reply(
        None,
        format!("backend declared {declared} requests for {expected} inputs"),
    )
}

fn result_count_error(reported: usize, requested: usize) -> VfError {
    VfError::invalid_reply(
        None,
        format!("backend reported {reported} results for {requested} requests"),
    )
}

impl<T> IntoIterator for ManyResults<T> {
    type Item = Result<T, VfError>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.results.into_iter()
    }
}

/// Deterministic fault injection used by backend contract and integration
/// tests. This module is absent from ordinary production builds.
#[cfg(feature = "test-faults")]
pub mod faults {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::{VfError, VfResult};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum OpenFaultPoint {
        BeforeDispatch {
            chunk: usize,
        },
        AfterReply {
            chunk: usize,
        },
        BeforeRegister {
            index: usize,
        },
        AfterRegister {
            index: usize,
        },
        BeforeCleanup {
            index: usize,
        },
        BeforeCloseDispatch {
            index: usize,
        },
        BeforeWriteChunk {
            chunk: usize,
        },
        AfterWriteChunk {
            chunk: usize,
        },
        BeforeWriteCompound {
            first: usize,
        },
        AfterWriteCompoundReply {
            first: usize,
        },
        AfterReadlinkCompoundReply {
            first: usize,
        },
        AfterLookupChunkReply {
            first: usize,
        },
        BeforeReadDirCompound {
            children: bool,
            first: usize,
            count: usize,
        },
        AfterReadDirCompoundReply {
            children: bool,
            first: usize,
            count: usize,
        },
        BeforeSmbCopyRequest {
            copied: u64,
        },
        AfterSmbCopyReply {
            copied: u64,
        },
        BeforeCopyCleanup {
            destination: bool,
        },
        BeforeCopySourceAttrs,
        AfterCopySourceAttrs {
            index: usize,
        },
        BeforeCopyCompound {
            first: usize,
        },
        AfterCopyCompoundReply {
            first: usize,
        },
        BeforeRenameCompound {
            first: usize,
        },
        AfterRenameCompoundReply {
            first: usize,
        },
        BeforeSetattrCompound {
            first: usize,
        },
        AfterSetattrCompoundReply {
            first: usize,
        },
        BeforeCreateCompound {
            first: usize,
        },
        AfterCreateCompoundReply {
            first: usize,
        },
        BeforeLinkCompound {
            first: usize,
        },
        AfterLinkCompoundReply {
            first: usize,
        },
        BeforeReadRepair {
            index: usize,
        },
        BeforeWriteRepair {
            index: usize,
        },
        BeforeReadChunk {
            chunk: usize,
        },
        BeforeSyncDispatch {
            index: usize,
        },
        AfterSyncReply {
            index: usize,
        },
        BeforeDirectoryPage {
            continuation: bool,
        },
        BeforeSizeQuery,
        BeforeFollowSymlink {
            index: usize,
        },
        BeforeSetPermissions {
            index: usize,
        },
        AfterNoFollowSetattrTypeCheck {
            index: usize,
        },
        BeforeRemoveCompound {
            first: usize,
        },
        AfterRemoveCompoundReply {
            first: usize,
        },
        BeforeRemoveType {
            index: usize,
        },
        BeforeRemoveChunk {
            first_name: Vec<u8>,
        },
        BeforeRemovePage {
            cookie: u64,
        },
        BeforeOpenChunk {
            chunk: usize,
        },
        AfterOpenChunkReply {
            chunk: usize,
        },
        BeforeOpenFinalize {
            index: usize,
        },
        AfterOpenReply {
            index: usize,
        },
        BeforeCloseItem {
            index: usize,
        },
        AfterCloseReply {
            index: usize,
        },
        BeforePathClose,
        BeforePathCloseBatch,
    }

    pub trait FaultInjector: Send + Sync {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()>;
    }

    /// An ordered, one-shot fault script. A configured fault fires only when
    /// its exact point is reached; tests can assert that no script entries
    /// remain afterward.
    #[derive(Debug)]
    pub struct FaultScript {
        remaining: Mutex<VecDeque<(OpenFaultPoint, VfError)>>,
        visited: Mutex<Vec<OpenFaultPoint>>,
    }

    impl FaultScript {
        pub fn one(point: OpenFaultPoint, error: VfError) -> Self {
            Self::new([(point, error)])
        }

        pub fn new(script: impl IntoIterator<Item = (OpenFaultPoint, VfError)>) -> Self {
            Self {
                remaining: Mutex::new(script.into_iter().collect()),
                visited: Mutex::new(Vec::new()),
            }
        }

        pub fn is_consumed(&self) -> bool {
            self.remaining
                .lock()
                .expect("fault script poisoned")
                .is_empty()
        }

        pub fn remaining(&self) -> Vec<OpenFaultPoint> {
            self.remaining
                .lock()
                .expect("fault script poisoned")
                .iter()
                .map(|(point, _)| point.clone())
                .collect()
        }

        pub fn visited(&self) -> Vec<OpenFaultPoint> {
            self.visited.lock().expect("fault script poisoned").clone()
        }
    }

    impl FaultInjector for FaultScript {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()> {
            self.visited
                .lock()
                .expect("fault script poisoned")
                .push(point.clone());
            let mut remaining = self.remaining.lock().expect("fault script poisoned");
            if remaining
                .front()
                .is_some_and(|(expected, _)| expected == point)
            {
                return Err(remaining.pop_front().expect("front exists").1);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod metadata_tests;

mod adb;
pub use adb::adb_layout;

mod budget;
pub use budget::TraversalBudget;

/// Lossy all-success convenience policy. This never schedules or retries I/O.
/// Error selection is by input position, not execution/completion order.
pub fn collect_vec_result<T>(
    results: crate::VecResult<T>,
    expected: usize,
) -> crate::VfResult<Vec<T>> {
    let results = results?;
    if results.len() != expected {
        return Err(VfError::invalid_reply(None, "invalid vector outcome count"));
    }
    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| result.map_err(|error| error.into_cause().with_index(index)))
        .collect()
}

/// Normalize an accepted subgroup to exactly one slot per input. Preserve known
/// slots, quarantine ambiguous excess output, and classify an outer rejection
/// as unattempted work. Whole-call structural rejection stays with the caller.
pub fn vec_result_slots<T>(
    result: crate::VecResult<T>,
    expected: usize,
) -> Vec<crate::OpResult<T>> {
    use crate::OpError;
    match result {
        Ok(mut results) if results.len() <= expected => {
            results.resize_with(expected, || {
                Err(OpError::unknown(
                    VfError::invalid_reply(None, "missing vector outcome"),
                    None,
                ))
            });
            results
        }
        Ok(_) => (0..expected)
            .map(|_| {
                Err(OpError::unknown(
                    VfError::invalid_reply(None, "excess vector outcomes"),
                    None,
                ))
            })
            .collect(),
        Err(cause) => (0..expected)
            .map(|_| Err(OpError::not_attempted(cause.clone())))
            .collect(),
    }
}

/// Move a borrowed destination's confirmed prefix into an owned error.
pub fn own_read_error(error: crate::OpError, offset: u64, mut buffer: Vec<u8>) -> crate::OpError {
    use crate::{FailureState, IoProgress, OpError, VfError};
    let (cause, state, progress) = error.into_parts();
    let progress = match progress {
        Some(IoProgress::Read(prefix))
            if prefix.is_buffered()
                && prefix.offset() == offset
                && prefix.bytes_read() <= buffer.len() =>
        {
            buffer.truncate(prefix.bytes_read());
            Some(IoProgress::Read(crate::api::ReadResult::owned(
                offset,
                buffer,
                prefix.eof(),
            )))
        }
        Some(_) => {
            return OpError::unknown(VfError::invalid_reply(None, "invalid read progress"), None);
        }
        None => None,
    };
    match state {
        FailureState::NotAttempted => OpError::not_attempted(cause),
        FailureState::Failed => OpError::failed(cause, progress),
        FailureState::Unknown => OpError::unknown(cause, progress),
    }
}

/// Add an earlier acknowledged prefix without weakening completion knowledge.
/// Invalid callback progress cannot erase already validated bytes.
pub fn add_write_prefix(
    error: crate::OpError,
    confirmed: usize,
    requested: usize,
) -> crate::OpError {
    use crate::{FailureState, IoProgress, OpError};
    if confirmed > requested {
        return OpError::unknown(
            VfError::invalid_reply(None, "WRITE prefix exceeds source"),
            None,
        );
    }
    let (cause, state, progress) = error.into_parts();
    let additional = match progress {
        None => 0,
        Some(IoProgress::Write { bytes_written }) if bytes_written <= requested - confirmed => {
            bytes_written
        }
        _ => {
            return OpError::unknown(
                VfError::invalid_reply(None, "invalid WRITE repair progress"),
                (confirmed != 0).then_some(IoProgress::Write {
                    bytes_written: confirmed,
                }),
            );
        }
    };
    let total = confirmed + additional;
    let progress = (total != 0).then_some(IoProgress::Write {
        bytes_written: total,
    });
    match state {
        FailureState::Unknown => OpError::unknown(cause, progress),
        FailureState::Failed => OpError::failed(cause, progress),
        FailureState::NotAttempted if total != 0 => OpError::failed(cause, progress),
        FailureState::NotAttempted => OpError::not_attempted(cause),
    }
}
