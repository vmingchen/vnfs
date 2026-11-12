# Healthy POSIX read profile and optimization — 2026-11-09

Baseline POSIX implementation: a0e613e. The NFS COPY fix is present in both
measured binaries; the compared production change is confined to PosixIo.
The prior result-envelope migration's measured 7.9% POSIX cost motivated this
profile. These new measurements compare the released migration with this
optimization; they do not establish parity with the pre-migration API.

## Profile and change

A pinned software cpu-clock:user profile at 999 Hz, with DWARF stacks, showed
allocation/zeroing and intermediate result processing alongside pread.
Baseline userspace self samples included malloc about 7.9%, memset about 9.8%,
and LocalBackend::read_into_results about 5.8%. See the flat before/after
profiles; inlining changes symbol attribution, so their percentages are not
an end-to-end timing comparison.

PosixIo now opts into the existing owned-destination executor interface and
fills each final Vec directly with pread. This removes the borrowed-buffer
adapter and intermediate result conversion for owned reads. Short reads retain
their actual lengths, errors retain independent siblings, and returned buffers
reuse their original allocations. The production change adds no new stable API or unsafe code.

A discarded lazy-index/empty-slot experiment saved one allocation per cohort,
but its approximately 1% healthy 64-file timing gain spanned parity without
allocation instrumentation. The smaller direct-owned implementation below is
the retained optimization.

## Paired measurements

rustc 1.93.1 (01f6ddf75 2026-02-11) (built from a source tarball); optimized builds with line tables, one pinned CPU
(3), warm page-cache files. Each file holds 4 KiB. Nine randomized
before/after pairs per case use 200,000/80,000/10,000/2,500 cohorts for
1/8/64/256 files, plus ten warm-up cohorts per process. No build, test or tracing
job ran during timing. Return counts and byte totals are checked for every run.
Mixed input replaces the middle file with a directory; both binaries perform
the same independent sibling work.

Ratios below are medians of paired after/before ratios, with bootstrap 95%
intervals for that median. Smaller is faster. Allocation-count instrumentation
uses a separate binary; native-allocator rows contain no counting allocator.

| Build | Files/cohort | Case | Wall ratio | 95% interval |
|---|---:|---|---:|---|
| Native allocator | 1 | success | 0.9008 | 0.8906–0.9281 |
| Native allocator | 8 | success | 0.9599 | 0.9558–1.0089 |
| Native allocator | 64 | success | 0.9457 | 0.9281–0.9615 |
| Native allocator | 256 | success | 0.9645 | 0.7854–1.1962 |
| Native allocator | 64 | mixed | 0.9501 | 0.9366–0.9659 |
| Allocation counts | 64 | success | 0.9554 | 0.9005–0.9885 |
| Allocation counts | 64 | mixed | 0.9525 | 0.9226–1.0001 |

The healthy 64-file native-allocator median improves 5.4%, with the interval
corresponding to 3.9–7.2% faster. Single-file improvement is about 9.9%.
The 8-file and especially 256-file samples are too variable to establish a
firm timing change. Raw pairs, CPU time, RSS and binary hashes are in results.json.

For a healthy 64-file cohort, instrumented allocation calls fall 74→71,
allocated bytes 285,184→279,552 (5.5 KiB less), and allocator peak-live delta
280,576→279,552. These are workload-specific measurements, not ABI or memory
ceiling guarantees.

Separate untimed strace runs count exactly 64,640 pread64 calls before and
after: 1,010 cohorts including warm-up × 64 inputs. Profiling and traced
durations are not used to calculate speedup. The scope is cached local reads.

## Reproduction

1. Create a temporary Cargo binary from harness.rs, with libc and serde_json
   dependencies and a path dependency on vnfs with posix/uring features.
   Declare empty before and alloc-metrics features. Use the same toolchain and
   CARGO_PROFILE_RELEASE_DEBUG=1 for both source snapshots.
2. Build --release --features alloc-metrics and --release separately. Preserve
   the resulting immutable binaries as /tmp/vnfs-posix-read-before-metrics,
   /tmp/vnfs-posix-read-before, and matching after names.
3. Create /tmp/vnfs-posix-read-profile-fixture/file-0 through file-255 with
   4,096 bytes each and an empty directory named directory. Write one allowed
   CPU number to /tmp/vnfs-posix-profile-cpu.
4. Run measure.py; its explicit /tmp paths identify inputs and output. Change
   those paths deliberately on another host. It checks result counts/bytes and
   emits randomized pairs with confidence intervals.
5. Run perf record separately with PROFILE_ITERATIONS=100000, cpu-clock:u,
   999 Hz and --call-graph dwarf,8192; run strace -c -e pread64 separately with
   PROFILE_ITERATIONS=1000. Pin the child to the same CPU in each case.

Test-only additions and documentation were finalized after the measured
binaries were frozen; the production implementation measured here is retained.
