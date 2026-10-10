# io_uring versus the POSIX VFSI backend

Measured on the development VM: ARM64, four vCPUs, Linux `7.0.0-28-generic`,
ext4 on `/dev/sda1`, optimized Cargo release profile. No artificial latency,
direct I/O, SQPOLL, or privileged cache drops. Both clients use the same
`FsClient`/`Vfsi` adapter and `LocalBackend` over the same local root.
The benchmark's `local/` labels select `vfsi-posix`.

```sh
cargo run --release -p vnfs --no-default-features --features uring,posix \
  --example uring_bench -- 200
```

## Validated before/after comparison

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

The optimized ring is 1.31× faster than POSIX on warm large reads, 1.60× on
advisory-cold large reads, and about 60× on advisory-cold small-file reads.
POSIX still wins warm small-file reads and buffered writes. The 2% small-write
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

Final counters: 3,915 waves, 3,915 ring entries, 167,040 submitted SQEs and
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
promises. Repeat on deployment storage and tune `Options::max_batch_bytes` and
`queue_depth` separately. Caller-facing read limits remain 16 MiB by default;
the 2 MiB window limits in-flight scratch, not total request size.

Open/close, namespaces, directories, append, and dependent writes remain ordinary
syscalls. Kernel-facing storage is still owned and copied, not a zero-copy API.
Future work should measure registered buffers or an owned-buffer API without
weakening submission-failure ownership, and profile buffered-write worker
scheduling before adding more concurrency. Earlier benchmark results are
superseded by this content-validated before/after run.
