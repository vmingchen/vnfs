# Local io_uring backend

Enable the optional Linux backend without bringing in the NFS native library:

```toml
[dependencies]
vnfs = { version = "0.0.24", default-features = false, features = ["uring"] }
```

The optional uring feature is available on Linux in the published vnfs crate.

```no_run
use vnfs::uring::{Uring};
use vnfs::files::{Vfsi, VfsiExt, OpenOp, OpenFlags, ReadOp};

# fn main() -> vnfs::Result<()> {
let fs = Uring::new("/data")?;
let files = fs.vopen(&[
    OpenOp::new("/file-1", OpenFlags::READ),
    OpenOp::new("/file-2", OpenFlags::READ),
])?;
let result = fs.vread(files.iter().map(|file| ReadOp::range(file, 0, 4096)),
                     Default::default());
let close = fs.close_files(files);
let results = result?;
close?;
for result in results { println!("{} bytes", result.bytes_read()); }
# Ok(())
# }
```

`vfsi-uring::connect` provides the same `Vfsi`/`VfsiExt` API as a standalone
crate. `vnfs::uring::Uring` keeps backend handles opaque. Neither needs Tokio.

### Opt-in warm-I/O fast paths

The default still routes independent descriptor I/O through the ring. To try
the adaptive prototype explicitly:

```no_run
use vnfs::uring::{Options, Uring};
# fn main() -> vnfs::Result<()> {
let options = Options::default().with_cached_reads(true).with_syscall_writes(true);
let (fs, telemetry) = Uring::with_telemetry("/data", options)?;
# let _ = (fs, telemetry);
# Ok(())
# }
```

`cached_reads(true)` probes the kernel page cache with `RWF_NOWAIT` twice per
cohort of 16 small reads (each at most 64 KiB). Two full hits select ordinary
positional reads for that cohort; a miss switches the remainder of the batch
to bounded ring waves. A mixed or evicted cohort can block on up to 14 unprobed
reads before checking again. Large reads always retain ring batching. Partial
and zero probe replies are completed through the ring, not mistaken for EOF;
unsupported filesystems disable probing for that client. This is an execution
policy over the existing kernel page cache, not a new client-side data cache.

`syscall_writes(true)` selects the shared ordered positional-write executor,
avoiding worker wakeups and the owned scratch copy for buffered writes. It
stops after a known error; it does not add atomicity or durability. Neither
fast path bypasses a poisoned executor. Both settings are disabled by default
pending the execution-policy decision; ring setup is still required.

## Execution and limits

Independent descriptor reads, positional writes, and fsyncs are submitted in
bounded waves. Adjacent writes to the same descriptor with contiguous offsets
share one kernel transfer; gaps, different descriptors, and reads are not merged.
Completion IDs restore input order, and merged short writes are mapped back to
each request's contiguous progress before retrying its remaining suffix. Default bounds:
256 SQEs and 2 MiB of in-flight transfer bytes (and at most that much reusable
scratch), configurable through
`vnfs::uring::Options::{queue_depth,max_batch_bytes}`. Queue depths must be
1..=4096. Application read budgets remain independently controlled by
`ReadOptions` and `ResourceLimits` (16 MiB by default). Large requests use bounded
contiguous chunks; short writes complete their remaining suffix, never skip gaps.

Open/close, metadata, directories, namespace changes, append and dependent writes
retain the existing local kernel implementation. This is deliberately **not**
an all-opcode backend. `write_files` still batches its descriptor-write phase.
Repeated cursor reads and overlapping writes, including hard-link aliases,
preserve ordering rather than racing. Nothing promises batch atomicity.

All completions are drained before returning an indexed CQE failure; other
independent writes in the submitted wave may already have completed. No later
wave or remaining short-write suffix is submitted after a known failure. Ring-level
submission failures poison the executor and do not replay writes. If completion
draining becomes impossible, the in-flight wave's owned buffers and
shared file descriptors are deliberately retained rather than freed while the
kernel may still reference them. This exceptional path leaks bounded resources;
final owned result allocations are bounded by the application read budget,
while scratch is bounded by the wave byte budget.
replace the client after such an error. Setup restrictions (kernel policy,
seccomp, unavailable opcodes) return an error, not silent syscall fallback.

In ring-only mode, owned `ReadOp::range` cohorts whose total requested storage
fits `max_batch_bytes` are read directly into final result allocations. The
executor owns these buffers until completion, avoiding the scratch-to-result
copy without changing the public API. Larger owned cohorts retain scratch:
on the measured ARM64 kernel, removing that copy everywhere regressed large
reads. Cache-aware selection continues using its existing borrowed-buffer path.
Repeated calls reuse progress, pending, SQE, grouping and completion storage.

This is not kernel zero-copy, O_DIRECT, SQPOLL, registered-buffer I/O, or an
asynchronous API: buffered reads still copy from the kernel page cache.
`ReadOp::into` caller buffers avoid application-owned output vectors but still
pass through bounded kernel-facing scratch to preserve failure safety.
Writes alone do not imply
durability; call `vfsync` when required.

`Uring::with_telemetry` returns counters for waves, actual submissions,
completions, peak in-flight transfer bytes, ring entries, arena growths, and
scratch-to-destination read-copy bytes. Ring counters
exclude syscall paths; cache probes/hits and selected syscall read/write
requests are reported separately. Namespace and dependency fallbacks remain
excluded. The executor waits for a complete wave rather than
waking for each completion; initialized scratch storage is reused only after
every submitted operation has drained. Cooperative task work avoids needless
completion IPIs on Linux 5.19+; older kernels retain ordinary rings. Submission
and waiting always happen on the same thread within a call, but clients may
move between threads after each call has drained.

## Reproduce the comparison

```sh
./scripts/test-ci-local.sh uring
cargo run --release -p vnfs --no-default-features --features uring,posix \
  --example uring_bench -- 200
```

An optional second argument selects an existing scratch parent directory.
Further arguments select `both|both-reverse|posix|uring`, a phase, queue
depth (default 256), and maximum scratch bytes (default 2097152). `all` runs
all 14 distinct cases, including owned, mixed-cache and write-shape cases.
`warm`, `cold`, and `write-shapes` select subsets. Every individual case can
also be selected, including `directories`, `cold-small-read`, `cold-large-read`,
`mixed-read`, the `owned-*` cases, `single-large-write`, and `many-files-write`.
For example,
`-- 300 target uring large-write 256 2097152` isolates the large buffered-write
workload for profiling; varying `max_batch_bytes` tests byte-window tuning
without changing library defaults.
The last argument selects `ring` (default), `cached` (cache-read probe only), or
`adaptive` (both opt-in fast paths). Use `both-reverse` to repeat measurements
with the executor order reversed, for example:

```sh
target/release/examples/uring_bench 300 target both all 256 2097152 adaptive
target/release/examples/uring_bench 300 target both-reverse all 256 2097152 adaptive
target/release/examples/uring_bench 20 target both mixed-read 256 2097152 adaptive
target/release/examples/uring_bench 300 target both write-shapes 256 2097152 adaptive
```
`write-shapes` compares equal 16 MiB payloads: one request, 128 contiguous
128 KiB requests on one file, and 256 independent 64 KiB requests on many files.
It retains the same alternating-payload and independent-content checks.
These are the defaults; trailing named options customize the geometry:

| Option | Default | Meaning |
|---|---:|---|
| `--files` | 256 | Independent small-file requests; also partitions the many-file write shape |
| `--small-bytes` | 4096 | Bytes per small-file request |
| `--large-bytes` | 16777216 | Total single-file payload and equal-payload write-shape budget |
| `--chunk-bytes` | 131072 | Request size within the large file; must divide `large-bytes` |
| `--order` | `sequential` | `random` deterministically permutes single-file range order, without overlap |
| `--seed` | 1 | Seed for the range permutation |
| `--max-case-bytes` | 67108864 | Maximum payload per vector call; increase explicitly for larger cases |
| `--json` | none | New report path; existing files are never overwritten |

Payload limits are validated before fixture creation. Validation/alternate
buffers can require approximately three times the payload, plus request/result
metadata. File count is capped at 65,536 and ranges at 1,048,576. Write-shape
comparisons require `large-bytes` to divide evenly among `files`.
Directory geometry remains a fixed 16-by-16 shared-syscall control.
The harness checks the process descriptor limit before creating fixtures: each
file needs a backend handle and an independent validation handle, plus a small
reserve. For large cohorts, raise only the benchmark shell's soft limit within
its existing hard limit (for example `ulimit -Sn 16384` for 4,096 files), or reduce
`--files`. No process or system limits are changed automatically.

For a small complete run, including actual ring I/O and both backends:

```sh
target/release/examples/uring_bench 5 target both all 16 65536 ring \
  --files 16 --small-bytes 4096 --large-bytes 65536 --chunk-bytes 4096 \
  --order random --seed 42 --json /tmp/uring-small-run.json
```

Build once, then sweep file counts and queue depth using distinct report paths.
Repeat with `both-reverse` to balance backend order; the harness pairs each
case across backends. Keep execution policy and allocation mode matched.

```sh
ulimit -Sn 16384 # Requires a sufficient existing hard limit; scoped to this shell.
for files in 1 16 256 4096; do
  for depth in 1 16 256; do
    target/release/examples/uring_bench 200 target both small-read "$depth" 2097152 ring \
      --files "$files" --json "/tmp/uring-read-${files}-${depth}.json"
  done
done
```

JSON reports preserve raw nanosecond samples, nearest-rank p50/p95/p99,
throughput, request rate and measured process CPU time. Low-sample tail estimates
are flagged; advisory-cold cases retain the 20-sample cap, so their p99 is not a
well-established tail estimate. Process CPU time is not system-wide CPU usage.
Each case uses a fresh executor. Counter deltas exclude warmups; case-local
peak bytes and total scratch growth include warmups and are labelled accordingly.
POSIX records have null ring telemetry rather than implying zero ring cost.
Environment records include runtime source revision/dirty status, executable
SHA-256, kernel/architecture, filesystem mount information and options.
Runtime source/compiler metadata does not prove how the binary was built;
retain the binary, build command and source diff alongside important results.
Reports are written only after every selected case succeeds; a failed run may
leave an empty reserved report file, not a successful benchmark record.

The `owned-*` phases use `ReadOp::range` and include result allocation in timing;
ordinary read phases use reusable caller buffers. Compare each phase against
the same phase, not owned allocations against reusable-buffer timings.
The default is `target`, because `/tmp` can be RAM-backed. The benchmark uses
identical `FsClient` adapters over the same root, comparing `vfsi-posix` against
`vfsi-uring`, both over shared `LocalBackend` machinery. It validates
distinct file/range patterns, poisons read buffers before each call, changes
write payloads every round, and independently reads written files through the
standard filesystem API. Validation and source reset stay outside timing. It
keeps opens/closes outside timed I/O, warms up five rounds, and reports
per-vector latency percentiles. Directory listings retain the same implementation.
Writes exclude fsync. Advisory-cold reads use `POSIX_FADV_DONTNEED` after syncing,
outside timing; this does not evict device/controller caches or guarantee cold
physical media. See `docs/uring-benchmark.md` in the repository for results and
environment. Do not expect io_uring to win on every workload.
