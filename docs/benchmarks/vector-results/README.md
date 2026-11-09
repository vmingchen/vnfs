# VecResult migration measurements

The report compares baseline a907e28 with the unreleased migration on the
same VM, using optimized builds and global allocation instrumentation.
Seven randomized before/after pairs run on one pinned allowed CPU. Each process
warms ten cohorts, then measures 1,000 cohorts of 64 positional 4 KiB reads.
The mixed case replaces input 32 with an open directory, which returns EISDIR.
Memory uses allocator peak-live bytes and /proc/self/status VmHWM after exec;
getrusage maxrss alone includes the launcher high-water mark on this host.

The harness uses the before Cargo feature only for the old aggregate signatures;
both versions execute the same workload. The old aggregate failure exposes no
successful data, while the migration returns 63 successful sibling reads per
failed cohort. In POSIX the old implementation stops syscall execution at the
first failure: a separate strace count observes 33,330 versus 64,640 pread64
calls (including warm-up). Its mixed timing ratio therefore
compares different amounts of work and must not be called result-envelope
overhead. io_uring submits and completes exactly 64,640 requests in 1,010
waves on both versions, in both cases.

This is a small cached local-read benchmark with allocator instrumentation.
It does not establish network-backend latency, production throughput, or an
application-wide memory ceiling. The report deliberately includes regressions:
healthy POSIX median paired wall time rises about 7.9%; io_uring about 3.2%
(with a bootstrap interval spanning parity). io_uring allocation calls fall by
four per measured cohort. Required per-input result storage raises peak-live
allocation by roughly 1--4%; process RSS medians rise about 124--132 KiB.
Do not describe the migration as universally faster.

Reproduction:
1. Archive baseline a907e28 into a disposable directory; preserve the dirty
   migration checkout independently.
2. Create two temporary Cargo binary projects using harness.rs as src/main.rs.
   Give both libc=0.2 and serde_json=1 dependencies, and a path dependency on
   the respective checkout's crates/vnfs with posix and uring features. Declare
   an empty before feature in the harness package.
3. Build the baseline with --release --features before and the migration with
   --release using the same Rust toolchain, target directory and environment.
4. Create a fresh fixture containing file-0 through file-63, each 4,096 bytes,
   and an empty directory named directory. Both binaries take arguments
   FIXTURE (posix|uring) (success|mixed) and print one JSON object.
5. Alternate pinned runs of the two immutable binaries in randomized order.
   Keep raw pairs, binary hashes, and bootstrap intervals as in the report.
   Count syscalls in separate strace runs; never time the traced executions.

The original measured binaries' hashes are recorded in the JSON report.
The formatted archived harness only consolidates its old/new API branches and silences
a discarded warm-up Result warning; it was not used to regenerate that report.

On this aarch64 target, VfError remains 32 bytes, OpError is 48 bytes,
OpResult<()> is 48 bytes, and OpResult<ReadResult> is 48 bytes versus the
40-byte ReadResult. Attrs and OpResult<Attrs> are both 144 bytes. These are
measured layouts, not cross-platform ABI guarantees. Success slots require no
individual allocation; optional progress is boxed only on an error carrying it.


## Network backend control

[The remote report](remote-2026-11-09.json) and
[remote harness](remote-harness.rs) compare current main 7de3f4e with the reviewed
0.2.0 / vnfs 0.0.32 source; both use SMB SDK 0.42.1. These runs followed the retained SMB
session guard and std_io fixes. No compilation or other test jobs ran during
the final timing pairs. Seven randomized pinned pairs use 200 cohorts of 64
positional 4 KiB reads after ten warm-up cohorts.

| Healthy backend | Median paired wall ratio after/before | Bootstrap 95% interval | Data-phase RPCs/commands before and after |
|---|---:|---:|---|
| NFS 4.1 | 0.973 | 0.877–1.227 | 464 / 464 measured RPCs |
| NFS 4.2 | 0.988 | 0.955–0.992 | 464 / 464 measured RPCs |
| SMB | 1.035 | 0.922–1.097 | 13,440 / 13,440 READ commands including warm-up |

NFS 4.1 and SMB timing intervals span parity. NFS 4.2 shows a small cached-read
improvement in this control; do not infer a general speedup. NFS allocation
calls fall from 69,960 to 58,020 (17.1%), allocated bytes from 121,560,960 to
70,157,760 (42.3%), and peak-live bytes from 543,680 to 304,949 (43.9%).
SMB healthy allocation count is essentially unchanged. The raw report retains
CPU, allocation, peak-live and process RSS results, including variability;
process RSS is not a per-request memory measurement.

The mixed remote case closes input 32 before measuring. The baseline rejects
the entire vector locally and issues zero data RPCs. The migration returns
12,600 successful sibling reads and 200 item errors. NFS performs 464 measured
RPCs; SMB performs 13,230 READ commands including warm-up. Its large mixed
time/memory ratios therefore compare real sibling I/O against no data I/O.
They are not result-wrapper overhead. Healthy cohorts return 12,800 successes
and no failures in each measured process.

SMB wire counts come from separate, untimed client-request captures on the
owned fixture port. Each accepted capture includes all 64 CREATE and 64 CLOSE
commands and reports zero kernel packet loss. READ and QUERY_INFO command
counts match between healthy versions. Failed or incomplete captures were
discarded. Wait for the recorder's listening notification before launching the
workload, use buffered capture with a sufficiently large ring, and drain its
tail before stopping it; stale capture files and immediate per-packet flushing
under competing load produced incomplete earlier controls.

To reproduce the network control, build two temporary binaries from
remote-harness.rs with libc, serde_json, vnfs, vfsi-sync and vfsi-smb dependencies
pointed at the respective source snapshots. Declare an empty before feature
and enable it only for the baseline build. The binary arguments are
ROOT (nfs41|nfs42|smb) (success|mixed). Supply VFSI_NFS_SERVER and an isolated
NFS export subtree, or VFSI_SMB_SERVER/SHARE/USERNAME/PASSWORD/DOMAIN and an
isolated Samba share. Each fixture contains file-0 through file-63 with the
corresponding repeated byte value and length 4,096. Use NFS minor versions 1
and 2 against the same server; select SMB ROOT=/.
Run without tracing for timing, then count client SMB commands with tcpdump
and tshark separately. Include warm-up commands in the expected wire total.
The formatted source preserves the measured algorithm; the report records
hashes of the actual measured optimized binaries.

These cached loopback fixtures do not establish WAN throughput or application
latency. NFS OPEN-adjacent GETFH safety has a separate wire cost in create/read
pipelines; these positional-read controls do not erase that cost.
