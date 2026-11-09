//! Temporary path I/O uses one owner, so later OPENs replace earlier stateids.
#![allow(non_upper_case_globals)]
use super::{CloseOp, NfsFh};
use nfsv41_sys::stateid4;

/// Retain only owners not retired by a successful inline CLOSE. Healthy
/// full compounds borrow reply handles and allocate no cleanup-owner entries.
pub(super) fn record_path_open_owners(
    response: &crate::compound::CompoundRes,
    opened: &mut Vec<(NfsFh, stateid4)>,
) {
    use nfsv41_sys::{nfs_opnum4_NFS4_OP_CLOSE, nfs_opnum4_NFS4_OP_GETFH, nfs_opnum4_NFS4_OP_OPEN};
    let mut pending = None;
    for index in 0..response.nops() {
        if response.op_status(index) != 0 {
            break;
        }
        match response.op(index).resop {
            nfs_opnum4_NFS4_OP_OPEN => {
                if let Some((fh, stateid)) = pending.take() {
                    opened.push((NfsFh::from_nfs_fh(response.getfh(fh)), stateid));
                }
                if index + 1 < response.nops()
                    && response.op(index + 1).resop == nfs_opnum4_NFS4_OP_GETFH
                    && response.op_status(index + 1) == 0
                {
                    pending = Some((index + 1, response.open(index).stateid));
                }
            }
            nfs_opnum4_NFS4_OP_CLOSE => {
                if let Some((fh, _)) = pending.take() {
                    let closed = response.getfh_bytes(fh);
                    // Reopening with the shared path owner replaces its
                    // stateid. Closing also retires older observations.
                    opened.retain(|(fh, _)| fh.as_bytes() != closed);
                }
            }
            _ => {}
        }
    }
    if let Some((fh, stateid)) = pending {
        opened.push((NfsFh::from_nfs_fh(response.getfh(fh)), stateid));
    }
}

/// Only for OPENs sharing the temporary path owner. Independent public OPEN
/// instances of the same object must each retain their own CLOSE.
pub(crate) fn latest_path_open_closes(opens: &[(NfsFh, stateid4)]) -> Vec<CloseOp> {
    // The common single-file/single-compound path needs no hash allocation.
    let capacity = if opens.len() > 1 { opens.len() } else { 0 };
    let mut seen = std::collections::HashSet::with_capacity(capacity);
    let mut closes: Vec<_> = opens
        .iter()
        .rev()
        .filter(|(fh, _)| opens.len() == 1 || seen.insert(fh))
        .map(|(fh, stateid)| CloseOp {
            fh: fh.clone(),
            stateid: *stateid,
        })
        .collect();
    // Selection scans backwards to keep the latest stateid, while CLOSEs
    // retain the input order of those selected observations.
    closes.reverse();
    closes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_uses_last_observation_even_when_stateid_sequence_wraps() {
        let first = NfsFh::test_from_bytes([1]);
        let second = NfsFh::test_from_bytes([2]);
        let state = |seqid| stateid4 {
            seqid,
            other: [7; 12],
        };
        let closes = latest_path_open_closes(&[
            (first.clone(), state(u32::MAX)),
            (second.clone(), state(3)),
            (first.clone(), state(0)),
        ]);
        assert_eq!(closes.len(), 2);
        assert_eq!(closes[0].fh, second);
        assert_eq!(closes[0].stateid.seqid, 3);
        assert_eq!(closes[1].fh, first);
        assert_eq!(closes[1].stateid.seqid, 0);
        assert!(latest_path_open_closes(&[]).is_empty());
        assert_eq!(latest_path_open_closes(&[(first, state(1))]).len(), 1);
    }
}
