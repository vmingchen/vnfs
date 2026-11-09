//! Contiguous completion of acknowledged short NFS wire operations.
use super::{RpcError, RpcResult, checked_offset};
use nfsv41_sys::{nfsstat4_NFS4ERR_IO, stable_how4_FILE_SYNC4};
#[cfg(test)]
use vfsi_core::{FailureState, IoProgress};
use vfsi_core::{OpError, OpResult, VfError};

pub(super) fn complete_read_suffix<C>(
    client: &mut C,
    caller: usize,
    offset: u64,
    count: usize,
    mut read: impl FnMut(&mut C, u64, usize) -> RpcResult<(Vec<u8>, bool)>,
    mut on_data: impl FnMut(&[u8]),
) -> RpcResult<bool> {
    checked_offset(offset, count, caller)?;
    let mut received = 0;
    let mut eof = false;
    while received < count && !eof {
        let remaining = count - received;
        let (data, end) = read(client, checked_offset(offset, received, caller)?, remaining)
            .map_err(|error| error.with_op_index(caller))?;
        if data.len() > remaining || (data.is_empty() && !end) {
            return Err(RpcError::op(caller, nfsstat4_NFS4ERR_IO));
        }
        received += data.len();
        on_data(&data);
        eof = end;
    }
    Ok(eof)
}

pub(super) fn write_rpc_error(error: RpcError) -> OpError {
    let cause = vfsi_core::error_from_rpc(error, None);
    if cause.is_transport() {
        OpError::unknown(cause, None)
    } else {
        OpError::failed(cause, None)
    }
}

pub(super) use vfsi_core::internal::add_write_prefix;

pub(super) fn complete_write_suffix<C>(
    client: &mut C,
    offset: u64,
    data: &[u8],
    mut write: impl FnMut(&mut C, u64, &[u8]) -> OpResult<(u32, u32)>,
) -> OpResult<u32> {
    checked_offset(offset, data.len(), 0)
        .map_err(|error| OpError::not_attempted(vfsi_core::error_from_rpc(error, None)))?;
    let mut written = 0;
    let mut committed = stable_how4_FILE_SYNC4;
    while written < data.len() {
        let (count, stable) = write(client, offset + written as u64, &data[written..])
            .map_err(|error| add_write_prefix(error, written, data.len()))?;
        let count = count as usize;
        let error = if count > data.len() - written || stable > stable_how4_FILE_SYNC4 {
            Some(OpError::unknown(
                VfError::invalid_reply(None, "invalid WRITE repair completion"),
                None,
            ))
        } else if count == 0 {
            Some(OpError::failed(
                VfError::nfs(None, nfsstat4_NFS4ERR_IO),
                None,
            ))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(add_write_prefix(error, written, data.len()));
        }
        written += count;
        committed = committed.min(stable);
    }
    Ok(committed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_repairs_only_unread_suffix_at_actual_offsets() {
        let mut calls = Vec::new();
        let mut output = b"prefix".to_vec();
        let eof = complete_read_suffix(
            &mut calls,
            4,
            7,
            7,
            |calls, offset, count| {
                calls.push((offset, count));
                Ok((vec![offset as u8; count.min(3)], false))
            },
            |data| output.extend_from_slice(data),
        )
        .unwrap();
        assert!(!eof);
        assert_eq!(calls, [(7, 7), (10, 4), (13, 1)]);
        assert_eq!(&output[..6], b"prefix");
        assert_eq!(&output[6..], &[7, 7, 7, 10, 10, 10, 13]);
    }
    #[test]
    fn read_stops_at_eof_and_accepts_empty_eof() {
        let mut calls = 0;
        let mut output = Vec::new();
        assert!(
            complete_read_suffix(
                &mut calls,
                3,
                0,
                5,
                |calls, _, _| {
                    *calls += 1;
                    Ok((
                        if *calls == 1 {
                            b"ab".to_vec()
                        } else {
                            Vec::new()
                        },
                        *calls > 1,
                    ))
                },
                |data| output.extend_from_slice(data)
            )
            .unwrap()
        );
        assert_eq!(output, b"ab");
        assert_eq!(calls, 2);
    }
    #[test]
    fn read_rejects_zero_progress_and_oversized_payloads() {
        for length in [0, 4] {
            let error = complete_read_suffix(
                &mut (),
                9,
                0,
                3,
                |_, _, _| Ok((vec![0; length], false)),
                |_| panic!("invalid data must not escape"),
            )
            .unwrap_err();
            assert_eq!(error.status, nfsstat4_NFS4ERR_IO);
            assert_eq!(error.op_index, 9);
        }
    }
    #[test]
    fn read_preserves_repair_failure_and_never_replays_prefix() {
        let mut calls = Vec::new();
        let mut output = Vec::new();
        let error = complete_read_suffix(
            &mut calls,
            7,
            10,
            5,
            |calls, offset, _| {
                calls.push(offset);
                if calls.len() == 1 {
                    Ok((b"ab".to_vec(), false))
                } else {
                    Err(RpcError::transport("lost repair"))
                }
            },
            |data| output.extend_from_slice(data),
        )
        .unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.op_index, 7);
        assert_eq!(calls, [10, 12]);
        assert_eq!(output, b"ab");
    }
    #[test]
    fn write_repairs_only_missing_suffix_and_combines_stability() {
        let mut calls = Vec::new();
        let stable = complete_write_suffix(&mut calls, 7, b"abcdefg", |calls, offset, data| {
            calls.push((offset, data.to_vec()));
            Ok((
                data.len().min(3) as u32,
                if calls.len() == 2 {
                    0
                } else {
                    stable_how4_FILE_SYNC4
                },
            ))
        })
        .unwrap();
        assert_eq!(stable, 0);
        assert_eq!(
            calls,
            [
                (7, b"abcdefg".to_vec()),
                (10, b"defg".to_vec()),
                (13, b"g".to_vec())
            ]
        );
    }
    #[test]
    fn write_rejects_zero_progress_and_oversized_counts() {
        for count in [0, 4] {
            let error = complete_write_suffix(&mut (), 0, b"abc", |_, _, _| {
                Ok((count, stable_how4_FILE_SYNC4))
            })
            .unwrap_err();
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
            if count == 0 {
                assert_eq!(error.state(), FailureState::Failed);
                assert_eq!(error.cause().raw_code(), nfsstat4_NFS4ERR_IO);
            } else {
                assert_eq!(error.state(), FailureState::Unknown);
                assert_eq!(
                    error.cause().transport_kind(),
                    Some(vfsi_core::TransportKind::InvalidReply)
                );
            }
        }
    }
    #[test]
    fn write_does_not_replay_after_repair_failure() {
        let mut calls = Vec::new();
        let error = complete_write_suffix(&mut calls, 5, b"abcd", |calls, offset, _| {
            calls.push(offset);
            if calls.len() == 1 {
                Ok((2, stable_how4_FILE_SYNC4))
            } else {
                Err(write_rpc_error(RpcError::transport("lost write repair")))
            }
        })
        .unwrap_err();
        assert!(error.cause().is_transport());
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 2 })
        );
        assert_eq!(calls, [5, 7]);
    }
    #[test]
    fn overflow_fails_before_repair_dispatch() {
        assert!(
            complete_write_suffix(&mut (), u64::MAX, b"a", |_, _, _| panic!("overflow")).is_err()
        );
        assert!(
            complete_read_suffix(
                &mut (),
                2,
                u64::MAX,
                1,
                |_, _, _| panic!("overflow"),
                |_| {}
            )
            .is_err()
        );
    }
    #[test]
    fn write_repair_failures_keep_prefixes_and_never_replay_after_rejection() {
        for state in [
            FailureState::Failed,
            FailureState::Unknown,
            FailureState::NotAttempted,
        ] {
            let mut calls = Vec::new();
            let error = complete_write_suffix(&mut calls, 11, b"abcdef", |calls, offset, data| {
                calls.push((offset, data.to_vec()));
                if calls.len() == 1 {
                    return Ok((2, stable_how4_FILE_SYNC4));
                }
                let cause = VfError::errno(99, libc::ENOSPC as u32);
                Err(match state {
                    FailureState::Failed => {
                        OpError::failed(cause, Some(IoProgress::Write { bytes_written: 1 }))
                    }
                    FailureState::Unknown => {
                        OpError::unknown(cause, Some(IoProgress::Write { bytes_written: 1 }))
                    }
                    FailureState::NotAttempted => OpError::not_attempted(cause),
                })
            })
            .unwrap_err();
            assert_eq!(calls, [(11, b"abcdef".to_vec()), (13, b"cdef".to_vec())]);
            assert_eq!(error.cause().index(), None);
            assert_eq!(error.cause().raw_code(), libc::ENOSPC as u32);
            assert_eq!(
                error.state(),
                if state == FailureState::Unknown {
                    state
                } else {
                    FailureState::Failed
                }
            );
            assert_eq!(
                error.progress(),
                Some(&IoProgress::Write {
                    bytes_written: if state == FailureState::NotAttempted {
                        2
                    } else {
                        3
                    },
                })
            );
        }
    }

    #[test]
    fn write_repair_malformed_progress_keeps_only_previously_validated_bytes() {
        for read_progress in [false, true] {
            let mut calls = 0;
            let error = complete_write_suffix(&mut calls, 0, b"abcde", |calls, _, _| {
                *calls += 1;
                if *calls == 1 {
                    return Ok((2, stable_how4_FILE_SYNC4));
                }
                Err(OpError::failed(
                    VfError::errno(None, libc::EIO as u32),
                    Some(if read_progress {
                        IoProgress::Read(vfsi_core::api::ReadResult::buffered(0, 1, false))
                    } else {
                        IoProgress::Write { bytes_written: 4 }
                    }),
                ))
            })
            .unwrap_err();
            assert_eq!(calls, 2);
            assert_eq!(error.state(), FailureState::Unknown);
            assert_eq!(
                error.cause().transport_kind(),
                Some(vfsi_core::TransportKind::InvalidReply)
            );
            assert_eq!(
                error.progress(),
                Some(&IoProgress::Write { bytes_written: 2 })
            );
        }
    }
}
