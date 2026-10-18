# io_uring versus the POSIX VFSI backend

The October 10–11, 2026 sections are historical experiments; their measurements
are preserved unchanged. The [October 18 follow-up](#october-18-2026-bounded-wave-planning)
records a new profiling-driven scheduler optimization against commit `7dc2654`.
For application setup, use the [io_uring guide](../crates/vnfs/src/guides/uring.md).

## Summary and provenance

The current executor defaults to ring I/O, a 256-entry queue, and a 2 MiB byte
window. `cached_reads(true)` and `syscall_writes(true)` remain explicit opt-ins;
the adaptive measurements must not be advertised as ring-only results. Verify
defaults in [`vfsi-uring::Options`](../crates/vfsi-uring/src/lib.rs) when tuning.

- The initial tables use caller-provided read buffers. The later
  [owned small-read experiment](#bounded-owned-small-read-fast-path) includes
  allocation and removes the extra userspace scratch-to-result copy only for
  bounded owned cohorts. These are different workloads, not contradictory results.
- Neither path is a zero-copy API: ordinary kernel/filesystem copies remain.
- Pure ring buffered writes and warm borrowed small reads did not universally
  beat POSIX. The [adaptive policy](#all-workload-follow-up-opt-in-adaptive-execution)
  is an alternative measured separately, not a default change.
- Directory enumeration uses shared syscall machinery; it is a control, not
  evidence of io_uring directory acceleration. Buffered writes exclude fsync
  and are not durable-storage throughput measurements.

| Record | Landed implementation |
|---|---|
| Initial scratch arena, completion waiting, and 2 MiB window; October 10 | [`c0ac238`](https://github.com/vmingchen/vnfs/commit/c0ac238916fb5fc97c75e2f23ad26c9d199a5b50) |
| Large-write/adaptive experiments, rejected tuning, and bounded owned reads; October 11 | [`ac23e37`](https://github.com/vmingchen/vnfs/commit/ac23e37e8f7827827ddde100b32c517efb215652) |

These commits locate the landed changes and reports. The original comparisons
used intermediate working-tree binaries, not necessarily those complete commit
trees; exact binary provenance cannot be reconstructed from a commit label alone.
The raw `target/uring-write-profile` artifacts cited by the original report were
absent when checked on October 17. The tables and methodology remain, but the
old binaries/profiles are not a retained reproducibility archive.

## Setup and current reproduction

Measured on the development VM: ARM64, four vCPUs, Linux `7.0.0-28-generic`,
ext4 on `/dev/sda1`, optimized Cargo release profile. No artificial latency,
direct I/O, SQPOLL, or privileged cache drops. Both clients use the same
`FsClient`/`Vfsi` adapter and `LocalBackend` over the same local root.
The benchmark's `local/` labels select `vfsi-posix`.

```sh
cargo run --release --locked -p vnfs --no-default-features --features uring,posix \
  --example uring_bench -- 200
```

Commands below exercise the checked-out harness; they do not reproduce an old
number merely by running on HEAD. Keep CPU, backend order, allocation mode,
cache procedure, and execution policy matched. Capture new results outside
build caches and identify the measured source before comparing them with history.

## Validated before/after comparison

This initial comparison belongs to the October 10 scratch-arena experiment;
the owned-read results later in this report supersede it only for that workload.

Warm results are medians of 200 calls after five warmups. Advisory-cold reads
use 20 measured calls after five warmups. Opens/closes, pattern preparation,
content validation, and cold-cache synchronization/eviction are outside timing.
Writes are buffered, not durable/fsync throughput. No tests, builds, or profiler
ran concurrently with these measurements.

The baseline uses the committed ring executor with its original 16 MiB window;
the optimized executor uses the reusable 2 MiB arena. Both runs use the updated
benchmark and the shared positional-read optimization, so the ring comparison
does not claim the metadata improvement twice. POSIX values are from the final
optimized run; its large-file times vary with the VM's CPU/cache state.

| Workload | POSIX | Ring before | Ring after | Ring improvement |
|---|---:|---:|---:|---:|
| Warm read: 256 × 4 KiB files | 97.4 µs | 129.8 µs | 117.2 µs | 1.11× |
| Warm write: 256 × 4 KiB files | 143.6 µs | 496.1 µs | 486.4 µs | 1.02× |
| Warm read: one 16 MiB file, 128 × 128 KiB ranges | 3,289.5 µs | 4,904.7 µs | 2,511.2 µs | 1.95× |
| Warm write: same large-file ranges | 2,146.8 µs | 3,872.5 µs | 2,823.1 µs | 1.37× |
| Advisory-cold read: 256 × 4 KiB files | 45,509.4 µs | 781.3 µs | 757.0 µs | 1.03× |
| Advisory-cold read: large-file ranges | 5,117.5 µs | 5,845.2 µs | 3,197.1 µs | 1.83× |

In this initial experiment, the optimized ring was 1.31× faster than POSIX on warm large reads, 1.60× on
advisory-cold large reads, and about 60× on advisory-cold small-file reads.
POSIX won warm small-file reads and buffered writes. The 2% small-write
change is effectively flat given VM variability, not a robust speedup claim.

Directory enumeration (16 directories × 16 files) averaged 853.2 µs POSIX
versus 893.3 µs ring. Both use the same ordinary syscall path; this is a control,
not an io_uring directory acceleration claim.

## Profile and changes

- A syscall trace found 1,220 `io_uring_enter` calls for 190 baseline waves.
  Tracing changes scheduling, so use it for counts, not throughput. Untraced
  counters in the matched 200-round baseline showed 31,561 entries for 870
  waves. Waiting for the entire wave eliminates repeated completion wakeups.
- Each SQE previously allocated scratch independently. A reusable owned arena
  now replaces those allocations while retaining both buffers and descriptors
  on catastrophic submission failures. The benchmark needs two arena growths
  instead of 167,040 per-operation scratch allocations.
- Reducing the default byte window from 16 MiB to 2 MiB improves the large-file
  copy working set. The queue remains 256 entries: 256 × 4 KiB still fits in
  one wave. A depth sweep of 256/64/16 showed that shrinking the operation queue
  hurts small-file batching; byte and operation limits should be tuned separately.
- `At` and `Cur` descriptor reads no longer fetch file size; only `End` needs
  current metadata. This benefits POSIX and ring equally. EOF still comes from
  the actual read, and EOF-relative offsets retain fresh size discovery.
- CPU profiling initially spent 81% of samples generating/checking benchmark
  patterns, outside I/O timing. Precomputed generations, poisoned read buffers,
  and byte-for-byte comparison retain validation without hiding executor costs.

Arena-optimization counters, before write coalescing: 3,915 waves, 3,915 ring entries, 167,040 submitted SQEs and
completions, two arena growths, and 2,097,152 peak scratch bytes. More bounded
waves but fewer ring entries and an 8× smaller scratch window.

Focused profiling (may require profiler privileges; no system settings changed):

```sh
target/release/examples/uring_bench 100 target uring warm 256
sudo perf record -e cpu-clock -g -- \
  target/release/examples/uring_bench 100 target uring warm 256
sudo perf report --stdio --no-children --call-graph none
strace -f -c -e io_uring_enter,statx,pread64,pwrite64 \
  target/release/examples/uring_bench 30 target uring warm 256
```

The focused post-change CPU profile still shows kernel copies (~35% of samples),
userspace copying (~10%), and io-worker wakeups (~7%). These explain why warm
buffered writes do not beat POSIX merely by batching. Percentages include
benchmark preparation/validation and are not library-only CPU budgets.

## Limits and next steps

Advisory eviction does not guarantee cold physical media; hypervisor/device
caches remain warm. These are indicative results from one VM, not performance
promises. Repeat on deployment storage and tune `Options::with_max_batch_bytes` and
`Options::with_queue_depth` separately. Caller-facing read limits remain 16 MiB by default;
the 2 MiB window limits in-flight scratch, not total request size.

Open/close, namespaces, directories, append, and dependent writes remain ordinary
syscalls. This initial borrowed-buffer path used owned scratch storage and
copies. The later bounded owned-read path avoids one userspace copy, but does
not eliminate kernel copies or provide a zero-copy API.
Future work should measure registered buffers or an owned-buffer API without
weakening submission-failure ownership. The following write-specific follow-up
addresses worker scheduling without adding concurrency. Earlier results are
historical measurements, not promises for every workload or machine.

## Large buffered-write follow-up

The write-only workload overwrites one 16 MiB file through 128 contiguous
128 KiB requests. Both versions use the same 2 MiB arena, queue depth 256, and
content validation; the baseline is the executor before adjacent-write merging.
Each run reports a median of 300 timed calls after five warmups. An alternating
baseline/optimized/optimized/baseline sequence, with no concurrent builds or
tests, produced:

| Executor | First run | Second run | Mean of run medians |
|---|---:|---:|---:|
| Before merging | 4,202.8 µs | 4,779.3 µs | 4,491.1 µs |
| After merging | 3,350.4 µs | 3,345.5 µs | 3,348.0 µs |

That is about **25% less time (1.34× throughput)** in this sequence. An earlier
alternating sequence showed a similar 1.36× improvement. Paired POSIX medians
were 2,886–2,968 µs: buffered ring writes still do not beat POSIX on this VM.
Repeating the entire workload matrix in both orders showed substantial VM
variation, including advisory-cold small reads ranging from 796–1,103 µs;
do not infer small percentage gains or regressions from one run.

Per 16 MiB call, actual submissions/completions fall from **128 to 8**, while
waves and ring entries remain eight, scratch stays bounded to 2 MiB, and the
arena grows once. Across 305 calls the counters are 2,440 waves/entries and
2,440 SQEs/CQEs, versus 39,040 SQEs/CQEs before merging. The full benchmark's
submissions fall from 167,040 to 142,440 with its waves and scratch unchanged.

Separate 500-call CPU profiles at 499 Hz found:

| Sampled CPU location | Before | After |
|---|---:|---:|
| Kernel copy from userspace | 41.4% | 52.6% |
| Userspace memcpy | 12.6% | 14.6% |
| Worker `try_to_wake_up` | 14.5% | Below the 1% report threshold |

Total sampled CPU fell from about 1.98 to 1.56 seconds. Percentages include
preparation/validation and change as total CPU changes; they are not wall-time
breakdowns. The remaining kernel-facing copy is intentional: borrowed caller
storage must not remain reachable by the kernel after an unrecoverable return.

The executor merges only adjacent input writes to the same descriptor at
contiguous offsets. It neither reorders writes nor merges reads, gaps, different
opens of the same inode, or ordered syscall fallbacks. A short completion is
distributed as a contiguous prefix across the original requests before retry;
a known error stops subsequent waves and suffix retries. Regression tests cover
short completions before/on/after a request boundary, zero progress, negative
CQEs, another failing request, gaps, reverse order, descriptor identity, tiny
byte windows, queue limits, and retained ownership after publication failure.

A 1/2/4/8 MiB window sweep yielded ring medians of 3,497/3,515/3,289/3,342 µs;
a repeated 2 MiB run took 3,267 µs. Larger windows did not establish a reliable
benefit, so the default remains 2 MiB rather than spending more memory.

Reproduce the focused measurements and tuning:

```sh
cargo build --release -p vnfs --no-default-features --features uring,posix \
  --example uring_bench
target/release/examples/uring_bench 300 target both large-write 256 2097152
sudo perf record -e cpu-clock -F 499 -g -- \
  target/release/examples/uring_bench 500 target uring large-write 256 2097152
sudo perf report --stdio --no-children --call-graph none --percent-limit 1
# Vary max_batch_bytes to compare byte windows.
```

Timing excludes opens/closes, payload preparation, independent content checks,
and fsync. This measures buffered overwrites, not durable disk throughput.

## All-workload follow-up: opt-in adaptive execution

The adaptive implementation landed with the ring-only default and two explicit switches:
`cached_reads(true)` requires two full probe hits per cohort of 16 small reads
before using ordinary positional syscalls; the first miss sends the
remaining reads through bounded ring waves. Large reads always use the ring.
`syscall_writes(true)` uses the existing ordered write executor rather than
paying for worker scheduling and an extra scratch copy. This is a hybrid
execution policy, **not a claim that pure io_uring beats POSIX at everything**.

Four complete runs used POSIX/ring order, reversed order twice, then original
order. Each reports medians of 300 warm calls and 20 advisory-cold calls;
directories report a mean over 300 calls. The following are means of those
four per-run statistics, in microseconds:

| Workload | POSIX | Adaptive ring backend | POSIX / adaptive |
|---|---:|---:|---:|
| Warm small read | 98.1 | 98.6 | 0.995× |
| Warm small write | 153.7 | 155.6 | 0.988× |
| Warm large read | 4,512.0 | 3,117.3 | 1.45× |
| Warm large write | 2,684.0 | 2,669.6 | 1.01× |
| Directory enumeration | 876.4 | 866.4 | 1.01× |
| Advisory-cold small read | 47,118.1 | 885.0 | 53.24× |
| Advisory-cold large read | 5,544.1 | 3,517.4 | 1.58× |

Warm small reads/writes are within about 1.3% of POSIX, not demonstrated strict
improvements. Directory enumeration uses the identical shared syscall path;
its small difference is a control/VM-noise result, not ring acceleration. A
separate order-balanced 1,000-call one-probe small-read sweep ranged from 98.0–99.4 µs
adaptive versus 97.5–100.8 µs POSIX. These observations support practical parity,
not a universal or exact wall-clock guarantee.

Each full adaptive run recorded 2,665 waves/entries, 48,640 SQEs/CQEs, one
arena growth, and 2 MiB peak scratch. Fast paths were counted separately:
9,785 NOWAIT probes, 9,760 full probe hits, 68,320 ordinary reads, 117,120 logical
syscall writes, and 6,400 read requests routed to ring fallback. A cold batch
probes only its first request, not every file. The warm cohort may still block
on up to 14 unprobed reads if the namespace is mixed or pages are evicted.

The `mixed-read` phase keeps the first file warm and evicts the other 255.
One-probe cohorts regressed to 3,626–3,779 µs versus ring-only's 788–990 µs:
one hot file selected 15 blocking cold reads. Requiring two hits reduced the
adaptive result to 803–884 µs, versus POSIX's 46,483–46,830 µs, with zero
unprobed syscall reads. Each call had two probes, one hit, and 255 queued ring
reads. This does not eliminate every possible mixed-cache misprediction, but
checks the actual observed regression without timing-based heuristics.

Other changes retained in ring-only mode:

- Cache device/inode identity per pinned descriptor, not per pathname. This
  removes repeated fstats and the temporary identity map from overlap checks;
  size/timestamps remain fresh. Tests verify rename/replacement identity and
  existing hard-link ordering.
- Use cooperative task work on kernels supporting it, with ordinary-ring setup
  on `EINVAL` from older kernels. Calls always submit and wait on the same
  thread, and a regression moves the drained executor between threads.
- Write chaining did not improve matched runs and was removed. The final
  5,000-call ring-only small-write profile took 281 µs per vector versus 354 µs
  before identity-map removal/cooperative completion changes. CPU samples fell
  from about 2.34 to 1.92 seconds; completion IPI sending fell below the 1%
  report threshold. Sampling and VM variation prevent attributing this entire
  change to one mechanism.

Fault tests cover partial/zero cache-probe results, EOF tails, unsupported
NOWAIT, a miss at the next warm cohort, original result indexing, real errors,
poisoned executors, syscall-write prefix failure, and ring/adaptive parity.
The owned-buffer submission-failure protections remain unchanged. Ring setup
still must succeed, and both fast-path switches remain disabled by default
pending the execution-policy decision.

```sh
target/release/examples/uring_bench 300 target both all 256 2097152 adaptive
target/release/examples/uring_bench 300 target both-reverse all 256 2097152 adaptive
target/release/examples/uring_bench 1000 target both small-read 256 2097152 adaptive
target/release/examples/uring_bench 20 target both mixed-read 256 2097152 adaptive
# Replace adaptive with ring for ring-only, or cached for cache-read-only.
```

## Equal-payload write shapes and window sweep

The `write-shapes` phase holds the payload at 16 MiB while changing request
shape. Four order-balanced adaptive runs (300 measured calls plus five warmups
per shape, 2 MiB window) gave these means of per-run medians:

| Shape | POSIX µs | Adaptive µs | POSIX / adaptive |
|---|---:|---:|---:|
| One 16 MiB write | 2,496.1 | 2,507.8 | 0.995× |
| 128 contiguous 128 KiB writes, one file | 2,639.2 | 2,626.3 | 1.005× |
| 256 independent 64 KiB writes | 2,435.5 | 2,392.1 | 1.018× |

The earlier two-run, 150-call check put independent adaptive writes about 2.7%
behind POSIX. The four longer balanced runs did not reproduce that gap. This
supports practical parity across these warm buffered write shapes, not a strict
guarantee that every sample will beat POSIX. Each adaptive run recorded 117,425
logical syscall writes and zero ring submissions: the speed comes from using
the shared ordered executor, not faster io_uring write completions.

Ring-only still trails POSIX. A separate two-run, 150-call order-balanced
comparison after the allocation cleanup recorded means of 3,021.1 / 3,046.7 /
3,552.0 µs for the same three shapes versus POSIX's 2,589.9 / 2,756.6 / 2,395.5 µs.
It recorded 3,720 waves, 42,160 SQEs/CQEs, and 2 MiB peak scratch per run.

The 256 KiB / 8 MiB / 16 MiB ring-only window sweep before the allocation cleanup
used 150 measured calls and both executor orders. Mean per-run medians were:

| Window | One 16 MiB write µs | Contiguous writes µs | Independent writes µs |
|---|---:|---:|---:|
| 256 KiB | 4,719.9 | 4,942.0 | 5,051.3 |
| 8 MiB | 3,220.6 | 3,260.8 | 4,111.5 |
| 16 MiB | 3,386.1 | 3,487.8 | 4,507.4 |

Fewer waves do not necessarily mean faster buffered writes. At 16 MiB, the
single-file cases used one SQE each, but the larger arena still had to be copied
and the worker scheduled. The conservative 2 MiB default is retained; there
is no evidence here for increasing it globally. Peak scratch matched each
configured window and submission/completion counts matched in every run.

A 10,000-call adaptive small-write CPU profile found kernel copying (17.3%),
libc syscall cancellation handling (13.4%), and the kernel syscall entry point
(11.1%) among the largest costs. No ring worker work occurs on this path.
The shared positional-write adapter did still allocate unused alias ranges for
ordered executors and a second vector solely to retain already-known offsets.
Removing these saves two allocations totaling 10 KiB per 256-request ordered
batch (POSIX and adaptive); ring writes also avoid the 2 KiB offset vector.
Offsets now come from the existing validated executor requests. Alias checks,
append fallback, result-cardinality checks and failure indexing are unchanged.

An eight-run matched before/after small-write comparison (3,000 calls per run,
balanced executor and binary order) changed adaptive means from 155.0 to
154.1 µs and POSIX from 153.7 to 152.4 µs. These differences are too small relative
to VM variation to claim a measured speedup; the allocation reduction is the
verified benefit. Local, POSIX, real-ring fault/batching tests, smoke tests and
full-feature lint checks passed after the cleanup.

```sh
target/release/examples/uring_bench 300 target both write-shapes 256 2097152 adaptive
target/release/examples/uring_bench 300 target both-reverse write-shapes 256 2097152 adaptive
# Compare ring-only with different max_batch_bytes values, without changing defaults.
target/release/examples/uring_bench 150 target both write-shapes 256 8388608 ring
```

## Rejected worker-concurrency experiment

The VM's kernel reported a default bounded-worker limit of 16. Experimental
binaries capped it at one or four workers, leaving the unbounded-worker limit
unchanged. Each cap was compared with the saved current binary in binary-order
ABBA runs: 200 warm measurements and 20 advisory-cold measurements per run,
with all payload checks and actual SQE/CQE counters retained. Executor order
was POSIX first for the one-worker experiment and reversed for the four-worker
experiment. Means of the two per-binary run medians, in microseconds:

| Experiment | Small write | Large write | Cold small read |
|---|---:|---:|---:|
| Default, paired with one worker | 354.3 | 2,669.6 | 765.1 |
| One worker | 323.8 | 2,808.3 | 678.5 |
| Default, paired with four workers | 324.3 | 2,721.8 | 767.2 |
| Four workers | 326.8 | 2,705.4 | 878.6 |

One worker lowered small-write time by about 8.6% in its paired comparison,
but large writes took about 5.2% longer. Four workers did not improve small
writes and increased cold-small-read time by about 14.5%. These small samples
and the variation between control runs do not justify a global worker cap.
All runs recorded the same 3,915 waves, 142,440 submissions/completions, two
arena growths and 2 MiB peak scratch. No missing work explains the differences.

The caps and diagnostic prints were removed; the executor is byte-for-byte
identical to its pre-experiment source. No worker option or kernel-wide setting
was added. Worker tuning does not currently supply a verified alternative to
the opt-in hybrid policy for the warm-write regression. Ring-only defaults
remain unchanged, and all-workload default parity remains unproven.

## Bounded owned small-read fast path

Ring-only owned read cohorts fitting `max_batch_bytes` now use their final
result allocations directly. Caller-provided buffers retain scratch storage
for failure safety. Larger owned cohorts also retain the original allocation
and scratch dispatch path: selecting the fast path happens before constructing
executor-owned requests. The POSIX path is unchanged. No new dependency,
public vNFS request type, registered buffer, or execution-mode setting is needed.

The executor reuses pending requests, progress/error arrays, SQEs, groups and
completion slots. Cohort-sized arrays are retained only for queue-sized calls;
larger calls release them rather than retaining arbitrary metadata for the
client's lifetime. Published buffers and descriptors remain owned until every
completion drains; an undrainable wave retains those resources and poisons the
executor. Known CQE errors stop subsequent waves without replay.

An unrestricted direct-read experiment regressed 16 MiB owned reads: two warm
run medians were 6,075.1 and 5,257.3 µs, versus baseline controls of 3,656.0 and
3,959.7 µs. A CPU-clock profile attributed 67.8% of sampled CPU to kernel
copy-to-user, with no userspace scratch-copy hotspot. Deliberately touching
destination pages before submission did not restore throughput and was removed.
The precise microarchitectural cause was not isolated; neither page faults nor
worker scheduling is established as the cause. These CPU shares include setup
and validation and are not fractions of timed I/O latency.

Final comparisons ran after builds/tests finished, with binary order
baseline/optimized/optimized/baseline and both backend orders. Means of two
per-binary run medians, in microseconds (3,000 warm-small calls, 200 other warm
calls, 20 advisory-cold calls; five warmups each):

| Read workload | Baseline ring | Optimized ring |
|---|---:|---:|
| Owned, 256 × 4 KiB warm | 144.4 | 111.2 |
| Owned, 128 × 128 KiB warm | 3,387.3 | 3,467.6 |
| Owned, 128 × 128 KiB advisory cold | 3,650.2 | 3,553.8 |
| Borrowed, 256 × 4 KiB warm | 120.7 | 116.9 |
| Borrowed, 128 × 128 KiB warm | 2,865.5 | 2,951.1 |
| Borrowed, 256 × 4 KiB advisory cold | 806.0 | 804.3 |
| Borrowed, 128 × 128 KiB advisory cold | 3,311.5 | 3,355.5 |

Owned warm small reads took 23.0% less time; simultaneous POSIX controls averaged
117.1 µs in optimized runs, about 5% slower than ring reads. An earlier eight-run
comparison found about 20% lower ring latency. Each final owned-small run kept
3,005 waves/entries and 769,280 matching SQEs/CQEs, with zero scratch growth and
zero scratch-to-result copy bytes. Work was not removed to obtain the speedup.

Large and borrowed results remain near baseline, not a guarantee of identical
performance. Warm large owned reads were 2.4% slower while their POSIX controls
were 1.7% slower; borrowed warm large reads were 3.0% slower while POSIX controls
were 2.5% slower. These few-percent differences require longer isolated runs
before attributing a regression or speedup. The unrestricted direct-read
slowdown is no longer present. Ring-only warm borrowed reads and writes still
do not universally beat POSIX; the default execution policy remains unchanged.

The `owned-*` phases include output allocation inside the timed call; ordinary
read phases reuse caller buffers. Never compare those phases as if they had
identical allocation costs. Results use independently prepared distinguishable
file/range contents and retain the same validation, OPEN/CLOSE exclusion and
advisory eviction procedure. Raw final logs were written under
`target/uring-write-profile/owned-selected-*.log`, alongside earlier unrestricted
and page-touch experiment binaries/profiles. That directory is no longer present
on the VM; do not treat these paths as available evidence.

```sh
target/release/examples/uring_bench 3000 target both owned-read 256 2097152 ring
target/release/examples/uring_bench 3000 target both-reverse owned-read 256 2097152 ring
target/release/examples/uring_bench 200 target both owned-large-read 256 2097152 ring
target/release/examples/uring_bench 20 target both owned-cold-large-read 256 2097152 ring
```

At the recorded implementation, regression coverage asserted unchanged final allocation addresses, zero copy
bytes on the direct path, exactly one wave for 256 small owned reads, bounded
fallback waves, short/EOF results, mixed owned/borrowed public reads, original
failure indices and retention/poisoning after injected publication failure.
`./scripts/test-ci-local.sh uring smoke check` passed: 35 ring/public/example/doc
checks, 159 smoke tests, 16 script tests, workspace and detached adapter lint
checks. Those jobs took 4 / 3 / 2 seconds respectively on a warm build cache.
The 48 shared-local unit tests also passed separately. Live NFS/SMB suites were
not rerun for this local-executor-only change; 11 live fixture tests in smoke
remain ignored and are not counted as coverage.

## October 18, 2026: bounded-wave planning

Baseline: `7dc2654`, built with the release profile and `uring,posix` features.
The candidate changes only the executor's scheduling scans; queue/byte defaults,
buffer ownership, syscall-selection policy and write-coalescing behavior remain
unchanged. This experiment used the ARM64 four-vCPU development VM, Linux
`7.0.0-28-generic`, and the existing disk-backed `target` scratch parent.

### Profile and implementation

A 16 MiB file read as 4,096 independent 4 KiB ranges, with queue depth 16 and a
64 KiB window, exposed repeated scanning of completed jobs and the complete
error array for every wave. At 499 Hz, process CPU-clock sampling attributed
12.35% of samples to `Engine::execute`; annotated assembly put its hottest
instructions in these scans. The candidate keeps a monotonic first-pending
cursor and a failure flag, retaining per-request progress/error storage.
It neither reorders work nor changes submission or failure ownership.
The executor's sample share fell to 2.62% in the follow-up profile.

These profiles include preparation, buffer poisoning and validation outside
the timed region. In particular, `memcmp` and `memset` samples are not all
library costs. Do not use sampled shares as wall-time fractions or read
speedups from profiled run times. Unprofiled timings follow below.

The deterministic regression initially failed with **16,766 planning visits for
128 reads**. It now bounds planning visits linearly for reads and writes using
a one-entry queue and three-byte window, verifies distinct per-request data and
changed write payloads, and requires exactly 512 submissions/completions/waves
for the combined work. Test-only counting adds no release instrumentation.

### Matched measurements

Run the immutable baseline/candidate binaries in ABBA order, reversing backend
order for balanced controls. Figures below are means of per-run p50 latencies,
not aggregate request percentiles or confidence intervals. The initial ABBA
sequence used two runs per binary: 300 samples for range/large-cohort cases and
3,000 for ordinary small reads/writes, each following five warmups.

| Ring-only workload | Baseline | Candidate | Interpretation |
|---|---:|---:|---|
| 4,096 × 4 KiB ranges; depth 16, window 64 KiB | 4,553.0 µs | 4,044.7 µs | 11.2% lower latency, 1.13× throughput |
| Same ranges; default depth/window | 4,061.2 µs | 4,030.7 µs | Essentially flat |
| 4,096 × 4 KiB files; default depth/window | 5,321.5 µs | 5,248.5 µs | Small difference; not a robust speedup claim |

The depth-16 POSIX control changed from 4,876.8 to 5,010.0 µs; the ring gain is
not explained by a faster control. Every matched scenario retained identical
submission/completion/wave counts, read-copy bytes and peak transfer budgets.
The 300-sample depth-16 ring case still performs 1,228,800 SQEs/CQEs in 76,800
waves, with 65,536 peak transfer bytes and 5,033,164,800 read-copy bytes.

An initial owned-read difference did not reproduce in a longer eight-run
confirmation (four runs per binary). Owned 4 KiB ranges averaged 5,107.7 versus
5,048.5 µs, about 0.5% faster after normalizing against POSIX controls: flat in
practical terms. An eight-run, 5,000-sample warm-small-read confirmation averaged
113.1 versus 113.7 µs; POSIX-normalized throughput was 0.999×. No warm-small-read
speedup or robust regression is established.

Cold-I/O results need more caution. A separate full-matrix comparison initially
showed substantially slower cold small/mixed reads, despite identical I/O work.
After an eight-run matched check, a further ring-only ABBA sequence with 16 runs
per binary measured cold-small means of 774.5 versus 818.2 µs and mixed-read
means of 769.9 versus 812.6 µs. Individual medians ranged from 667–926 versus
719–1,007 µs for cold reads, and 726–847 versus 726–1,070 µs for mixed reads.
Cold-small caller CPU time was essentially unchanged (511.3 versus 510.4 µs
per call). The roughly 5–6% slower candidate wall-time means and broad spread
are retained as a residual performance concern, not dismissed as a proven
non-regression. These measurements do not establish all-workload parity.
An eight-run small-write confirmation was approximately flat against POSIX
controls (0.990× normalized throughput). No default tuning change follows from
these inconclusive cases.

### Remaining costs and reproduction

Warm borrowed small-read profiling still shows kernel copy-to-user, userspace
copying and request setup. Borrowed destinations retain their kernel-facing
scratch for catastrophic-failure safety; removing that copy was not part of this
change. System-wide small-write profiling found worker wakeups and kernel/user
copies; idle and unrelated work were also sampled. This does not justify a
worker-policy or default execution-mode change. Hybrid/syscall-write results
must remain separate from ring-only performance.

Raw JSON, logs, perf data, annotated assembly, and immutable binaries are retained
outside Cargo caches on the development VM at
`/home/ubuntu/uring-profile-2026-10-18.Bt2ZLt`. Source revision/status recorded by
the driver are runtime metadata: distinguish the `7dc2654` baseline from the
candidate scheduler patch, and use each report's executable SHA-256 to identify
the measured binary. These are single-VM observations, not deployment guarantees.

```sh
export CARGO_TARGET_DIR=target/merge-vnfs01
cargo build --release --locked -p vnfs --no-default-features \
  --features uring,posix --example uring_bench
target/merge-vnfs01/release/examples/uring_bench 300 target both large-read 16 65536 ring \
  --large-bytes 16777216 --chunk-bytes 4096 --json /tmp/uring-ranges-q16.json
# Repeat with both-reverse and alternate saved baseline/candidate binaries.
# Profile separately; do not compare perf-instrumented timings with untraced ones.
sudo perf record -e cpu-clock -F 499 -g -- \
  target/merge-vnfs01/release/examples/uring_bench 300 target uring large-read 16 65536 ring \
  --large-bytes 16777216 --chunk-bytes 4096
sudo perf report --stdio --no-children --call-graph none
```

Validation: the complete real-ring unit/fault, backend, public facade,
benchmark and guide selections passed, as did the repository smoke selection,
strict selected-target Clippy, formatting, and Rust 1.88 compatibility checks.
The new planning regression was observed failing before the optimization.
Live NFS/SMB tests are outside this executor-only change and were not rerun.
