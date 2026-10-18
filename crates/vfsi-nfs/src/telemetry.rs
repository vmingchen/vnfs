//! Opt-in counters shared only by explicitly selected NFS clients.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Cloneable diagnostic handle. Pass it to `NfsClientBuilder::telemetry` before
/// connecting. Clones aggregate; separate handles are independent. Attaching the
/// same handle to a pool aggregates its members. Connection handshakes are
/// excluded; operations and teardown after attachment are included, even after
/// reconnect. No callbacks, logging, or per-request allocations are introduced.
#[derive(Clone, Debug, Default)]
pub struct Telemetry(Arc<Counters>);

#[derive(Debug, Default)]
struct Counters {
    calls: AtomicU64,
    transport_errors: AtomicU64,
    nfs_errors: AtomicU64,
    operations: AtomicU64,
    request_bytes: AtomicU64,
    rpc_nanos: AtomicU64,
    max_operations: AtomicU64,
}

/// Cumulative counters, read without resetting any other consumer's view.
/// Concurrent snapshots are approximate, not an atomic multi-field transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// COMPOUND RPC attempts, including transport and NFS errors.
    pub calls: u64,
    pub transport_errors: u64,
    /// Decoded non-OK top-level NFS statuses (not transport failures).
    pub nfs_errors: u64,
    /// Requested operations, including SEQUENCE and failed attempts.
    pub operations: u64,
    /// Actual successfully encoded COMPOUND argument bytes. Excludes RPC,
    /// authentication and TCP framing; auth refresh may encode a call again.
    pub request_bytes: u64,
    /// Synchronous RPC time, including encode/wait/decode, excluding client lock
    /// wait, compound planning and reconnect handshakes.
    pub rpc_nanos: u64,
    pub max_operations: u64,
}

impl Telemetry {
    pub fn snapshot(&self) -> Snapshot {
        let c = &self.0;
        Snapshot {
            calls: c.calls.load(Ordering::Relaxed),
            transport_errors: c.transport_errors.load(Ordering::Relaxed),
            nfs_errors: c.nfs_errors.load(Ordering::Relaxed),
            operations: c.operations.load(Ordering::Relaxed),
            request_bytes: c.request_bytes.load(Ordering::Relaxed),
            rpc_nanos: c.rpc_nanos.load(Ordering::Relaxed),
            max_operations: c.max_operations.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn record(&self, operations: u64, bytes: u64, nanos: u64, status: Option<u32>) {
        let c = &self.0;
        c.operations.fetch_add(operations, Ordering::Relaxed);
        c.request_bytes.fetch_add(bytes, Ordering::Relaxed);
        c.rpc_nanos.fetch_add(nanos, Ordering::Relaxed);
        c.max_operations.fetch_max(operations, Ordering::Relaxed);
        c.transport_errors
            .fetch_add(u64::from(status.is_none()), Ordering::Relaxed);
        c.nfs_errors
            .fetch_add(u64::from(status.is_some_and(|s| s != 0)), Ordering::Relaxed);
        c.calls.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_aggregate_without_resetting_or_crossing_client_scopes() {
        let first = Telemetry::default();
        let second = Telemetry::default();
        std::thread::scope(|scope| {
            for copy in [first.clone(), first.clone()] {
                scope.spawn(move || {
                    for _ in 0..100 {
                        copy.record(3, 64, 10, Some(0));
                    }
                });
            }
        });
        first.record(7, 96, 20, None);
        first.record(2, 32, 30, Some(10001));
        let expected = Snapshot {
            calls: 202,
            transport_errors: 1,
            nfs_errors: 1,
            operations: 609,
            request_bytes: 12928,
            rpc_nanos: 2050,
            max_operations: 7,
        };
        assert_eq!(first.snapshot(), expected);
        assert_eq!(first.snapshot(), expected);
        assert_eq!(second.snapshot(), Snapshot::default());
    }
}
