//! Temporary path I/O uses one owner, so later OPENs replace earlier stateids.
use super::{CloseOp, NfsFh};
use nfsv41_sys::stateid4;

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
