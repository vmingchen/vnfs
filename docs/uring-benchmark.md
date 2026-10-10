# io_uring versus the POSIX VFSI backend

Measurement after the executor split on the development Linux VM: ARM64, Linux
`7.0.0-28-generic`, ext4 on `/dev/sda1`, optimized Cargo release profile.
No NFS server, artificial latency, direct I/O, SQPOLL or privileged cache drops.
Both clients use the same `FsClient`/`Vfsi` adapter and `LocalBackend` machinery
over the same local root. The benchmark's `local/` labels select `vfsi-posix`.

Command:

```sh
cargo run --release -p vnfs --no-default-features --features uring,posix \
  --example uring_bench -- 200
```

Each warm I/O result is the median of 200 vector calls after five warmups.
Advisory-cold reads use 20 measured calls after five warmups, synchronizing then
requesting page-cache eviction outside timing. Both backends start with identical
file/range-specific patterns. Read buffers are poisoned before each call, write
payloads change every round, and written contents are checked through independent
standard-filesystem reads. Opens/closes, setup, checksum verification and cold-cache preparation
are excluded from I/O timing. Writes are buffered, not durable/fsync throughput.

| Workload | POSIX | io_uring | POSIX time / ring time |
|---|---:|---:|---:|
| Warm read: 256 × 4 KiB files | 159.5 µs | 192.8 µs | 0.83× |
| Warm write: 256 × 4 KiB files | 192.1 µs | 553.4 µs | 0.35× |
| Warm read: one 16 MiB file, 128 × 128 KiB ranges | 3,933.5 µs | 5,467.2 µs | 0.72× |
| Warm write: same large-file ranges | 2,344.7 µs | 4,334.6 µs | 0.54× |
| Advisory-cold read: 256 × 4 KiB files | 46,224.1 µs | 769.4 µs | 60.08× |
| Advisory-cold read: large-file ranges | 5,102.9 µs | 5,908.2 µs | 0.86× |

Directory enumeration (16 directories, each containing 16 files) averaged
894.0 µs POSIX versus 863.7 µs ring. It uses the same ordinary syscall path,
so this is a control, not an io_uring directory acceleration claim.

Actual ring counters: 870 waves, 167,040 submitted SQEs, 167,040 completions,
16,777,216 peak scratch bytes. Independent requests really share submissions;
this is not a scalar syscall loop with an io_uring name.

## Interpretation and limits

Many small storage-backed reads benefit substantially from overlapping waits.
Warm large reads and buffered writes do not benefit in this implementation;
extra scratch allocation/copying, per-request metadata checks and completion
processing compete with already inexpensive page-cache syscalls. Large
sequential reads also benefit from the kernel's existing read-ahead.

An earlier `/tmp` experiment used tmpfs and showed io_uring slower on most warm
operations. It cannot establish storage cold-read performance. The numbers
above use ext4, but advisory eviction does not guarantee cold physical media,
and this VM's hypervisor/device caches remain warm. These are indicative results
from one VM, not general performance promises. Re-run on deployment storage.

Earlier runs, including the pre-split backend, are superseded by the
content-validated run above. No tests or builds ran concurrently
with this measurement. Validation is outside timing but changes CPU-cache state;
warm means kernel page-cache resident, not a guarantee of CPU-cache residency.

The current backend batches descriptor reads, independent positional writes
and fsyncs. Open/close, namespace, directory operations, append and dependent
writes are intentionally ordinary local operations. Future optimization should
first measure scratch-copy overhead and consider reusable/registered buffers
without weakening in-flight buffer ownership on submission failures.
