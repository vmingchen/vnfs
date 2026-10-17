# Complete-write scheduler benchmark

Measured on 2026-10-11 in `/home/ubuntu/vnfs` on the development VM: ARM64,
four allowed CPUs, Linux 7.0.0-28-generic, ext4 on `/dev/sda1`, Rust 1.93.1
(LLVM 21.1.8), Cargo release profile. Benchmark children were pinned to CPU 3.
No tests, builds or profiler ran concurrently with timed samples.

The comparison uses the original scheduler in commit
`4b9ff879b621cdb056f0cd20f2377e3d7fefc308` and the scheduler later committed in
[`2acffaa`](https://github.com/vmingchen/vnfs/commit/2acffaa0d0badb248787da39e817a007e9d616ea).
The measured `crates/vfsi-sync/src/client/write.rs` SHA-256, also present in that commit, is
`fcf8cefa51f5ecaef67148c6c755dba627918d89d2cc55709f63e0f95ca35936`.
The earlier allocation/layout changes are present in both versions.

This is a dated report, not a new benchmark of HEAD. Measurements and checked-in
raw samples are preserved; the reproduction commands below run the selected
checkout and must not be used to relabel these results as current measurements.

## Results

The main matrix has 35 cases, with nine paired samples per case. All paired
median speedups are above 1 in this run; 24 have a 95% interval entirely above
1.05. Eleven show smaller gains or uncertainty under that threshold. No case
has an interval entirely below 1. Independent POSIX writes are effectively flat.
These are measurements on one VM, not guarantees for other workloads or hosts.

For 256 overlapping writes, buffered POSIX time fell from 2,341.54 to
270.77 µs per batch (8.671× paired speedup). At 512 it fell from 16,447.93
to 811.39 µs (20.175×). Without file-data I/O, 1,024 conflicting requests fell
from 122,750.10 to 2,520.71 µs (48.841×). Mixed scheduling at 256 requests
improved by 1.131×. Ratios below are medians of paired ratios; rounding and
pairing mean they need not equal the ratio of the two independently computed
time medians.

| Backend / scenario | Requests | Before (µs/batch) | After (µs/batch) | Paired speedup | 95% interval |
|---|---:|---:|---:|---:|---:|
| sink / overlap | 1 | 0.22 | 0.19 | 1.105× | 1.082–1.161 |
| sink / overlap | 16 | 3.94 | 3.39 | 1.162× | 1.159–1.175 |
| sink / overlap | 64 | 52.36 | 21.06 | 2.472× | 2.460–2.500 |
| sink / overlap | 128 | 311.19 | 60.41 | 5.218× | 5.055–5.426 |
| sink / overlap | 256 | 2,259.06 | 181.44 | 12.511× | 11.702–12.730 |
| sink / overlap | 512 | 16,358.15 | 610.87 | 26.817× | 24.311–28.310 |
| sink / overlap | 1024 | 122,750.10 | 2,520.71 | 48.841× | 44.185–51.368 |
| sink / chain | 64 | 16.45 | 9.62 | 1.715× | 1.685–1.773 |
| sink / chain | 256 | 128.02 | 54.02 | 2.384× | 2.332–2.417 |
| sink / chain | 1024 | 1,522.89 | 465.87 | 3.288× | 3.240–3.319 |
| sink / scatter | 64 | 6.18 | 5.82 | 1.072× | 1.055–1.094 |
| sink / scatter | 256 | 43.06 | 38.23 | 1.125× | 1.066–1.156 |
| sink / scatter | 1024 | 482.00 | 401.18 | 1.201× | 1.198–1.232 |
| sink / independent | 1 | 0.21 | 0.19 | 1.083× | 1.013–1.142 |
| sink / independent | 16 | 2.10 | 1.90 | 1.107× | 1.063–1.121 |
| sink / independent | 64 | 8.88 | 8.51 | 1.043× | 1.004–1.054 |
| sink / independent | 256 | 35.34 | 34.33 | 1.030× | 1.011–1.056 |
| sink / overlap-short | 64 | 167.69 | 37.40 | 4.457× | 4.301–4.519 |
| sink / overlap-short | 256 | 8,636.95 | 250.19 | 34.624× | 34.131–34.850 |
| sink / independent-short | 64 | 13.55 | 12.69 | 1.064× | 1.045–1.093 |
| sink / independent-short | 256 | 49.94 | 49.28 | 1.018× | 0.998–1.144 |
| sink / mixed | 64 | 8.17 | 7.74 | 1.061× | 1.033–1.074 |
| sink / mixed | 256 | 42.91 | 37.90 | 1.131× | 1.126–1.136 |
| sink / append | 64 | 51.72 | 20.80 | 2.484× | 2.458–2.501 |
| sink / append | 256 | 2,268.25 | 183.60 | 12.209× | 11.118–12.676 |
| posix / overlap | 64 | 74.16 | 43.18 | 1.717× | 1.680–1.738 |
| posix / overlap | 256 | 2,341.54 | 270.77 | 8.671× | 8.471–8.859 |
| posix / overlap | 512 | 16,447.93 | 811.39 | 20.175× | 18.497–21.222 |
| posix / chain | 64 | 37.49 | 30.31 | 1.235× | 1.220–1.281 |
| posix / chain | 256 | 213.18 | 140.68 | 1.510× | 1.476–1.545 |
| posix / scatter | 64 | 24.80 | 24.39 | 1.019× | 1.004–1.049 |
| posix / scatter | 256 | 117.82 | 113.01 | 1.042× | 1.029–1.056 |
| posix / independent | 16 | 6.92 | 6.70 | 1.031× | 1.022–1.055 |
| posix / independent | 64 | 28.77 | 28.48 | 1.011× | 0.994–1.021 |
| posix / independent | 256 | 116.49 | 115.78 | 1.007× | 0.987–1.031 |

### Longer confirmation

Six selected cases were repeated with 21 paired samples and a 500 ms target
for the slower member. The same scheduler source and verified baseline binary
were used. All intervals stayed above 1; the independent POSIX control changed
only about 0.3%, so no meaningful improvement is claimed for that case.

| Backend / scenario | Requests | Before (µs/batch) | After (µs/batch) | Paired speedup | 95% interval |
|---|---:|---:|---:|---:|---:|
| sink / scatter | 1024 | 481.69 | 401.85 | 1.201× | 1.189–1.207 |
| sink / independent | 1 | 0.21 | 0.19 | 1.081× | 1.079–1.088 |
| sink / independent-short | 256 | 49.70 | 48.59 | 1.021× | 1.017–1.027 |
| sink / mixed | 256 | 42.78 | 37.87 | 1.132× | 1.125–1.135 |
| posix / overlap | 256 | 2,346.92 | 263.72 | 8.910× | 8.822–9.025 |
| posix / independent | 256 | 115.44 | 115.24 | 1.003× | 1.001–1.005 |

## Workloads and measurement

`crates/vnfs/examples/write_schedule_benchmark.rs` calls the actual generic
`FsClient::vwrite_all_native` API. The sink acknowledges writes, updates descriptor
end offsets and counts vector dispatches without file-data I/O. This isolates
library scheduling and allocation costs. It does not instrument request
projection counts inside timing. The POSIX backend writes real fixture files
on ext4 and verifies their final contents against input-order writes after every
sample. Both versions must return valid completion results and identical sink
dispatch counts.

| Scenario | Requests |
|---|---|
| `overlap` | One file, every request at offset 0, 32 bytes each. |
| `chain` | One file, starts every 32 bytes, length 64; neighboring ranges conflict. |
| `scatter` | One file, starts every 64 bytes, length 32; ranges do not conflict. |
| `independent` | One distinct file per request, offset 0, 32 bytes each. |
| `overlap-short` / `independent-short` | 128-byte requests; sink acknowledges at most 32 bytes, requiring four writes per request. |
| `mixed` | Four-request groups across two files, lengths 32/64/96/96; one request waits for two overlapping predecessors; sink acknowledges at most 32 bytes. |
| `append` | One append descriptor, 32 bytes per request; sink orders every earlier same-path write. |

Timed regions include complete-batch preflight, conflict construction,
scheduling, backend dispatch, reply validation, short-write retries, allocations
and result destruction. Paths, open handles, payloads and request vectors are
prepared beforehand. Verification calls, three warmup calls, opens/closes,
POSIX content readback and fixture cleanup are outside timing. Repeated POSIX
writes reuse buffered extents and do not call `fsync`. This measures buffered
call latency, not durable storage throughput. NFS/SMB throughput, cold storage,
large payloads and concurrent clients were not benchmarked.

The runner calibrates a common iteration count from the slower of two probes
(maximum 5,000,000 iterations), then alternates baseline/optimized and
optimized/baseline order. The main target is 200 ms for the slower sample; fast
optimized overlap samples therefore take less time. It reports separate time
medians and a median of paired speedups. The interval is a deterministic
10,000-resample percentile bootstrap of the paired median. All samples are
retained, with no outlier removal; the interval describes this sample set and
does not account for every source of VM or deployment variation.

## Reproduction and records

Run from the repository on Linux with the recorded baseline commit available.
The runner compares that baseline with the current working tree; use the landed
commit above in a separate checkout to study the historical implementation.
Offline dependencies must already be cached:

```sh
python3 scripts/benchmark-write-scheduler.py --samples 9 --target-ms 200
```

The runner builds both versions using the identical example and manifest entry:

```sh
cargo build --release --locked --offline -p vnfs \
  --no-default-features --features posix,test-faults \
  --example write_schedule_benchmark --message-format=json -vv
```

Use `--case BACKEND/SCENARIO/COUNT` repeatedly for exact selections, `--cpu` to
choose another allowed CPU, and `--output` for a new output directory. For the
confirmation selection, use `--samples 21 --target-ms 500` with the six rows
above. Cached dependencies are required for `--offline`; `taskset` supplies CPU
affinity. `--reuse-baseline PATH` requires matching commit, harness, compiler,
Cargo version and recorded build configuration. It verifies the saved executable
hash and compares Cargo artifact profiles before timing. Results without verified
configuration metadata require rebuilding without `--reuse-baseline`.

Configuration metadata records Rust flags, Cargo profile/build/target environment
overrides, compiler wrappers, workspace profiles and hashes of Cargo configuration
files (including ancestor, Cargo-home and included files). Config file contents
and registry credentials are not retained. The check conservatively requires
matching inputs; additional inherited configuration in a baseline snapshot can
cause rejection. The runner copies the executable reported by Cargo's JSON
artifact event, including a configured target's path. It checks for configuration
changes during the run. Python 3.12+ and `taskset` are required.

The runner extracts the baseline with `git archive`, and uses separate
persistent build caches under `target/write-scheduler-build/`. It records both
executable hashes and requires them to differ. An initial comparison using a
shared Cargo cache reused the same artifact for both source trees; that run was
discarded. A later lazy sparse-graph experiment slowed scatter writes; it was
replaced before the final runs. Graph slots are now allocated before the hot
pairwise loop for batches of two or more requests, and the first successor is
stored inline. Scalar writes require no graph arrays. The inline slot is 32
bytes on this build versus 24 for a `Vec<usize>`, and adds an eight-byte
dependency counter per graph node. It avoids a separate edge allocation for
nodes with just one successor. These are allocation/CPU tradeoffs, not a claim
that every graph representation is smaller.

Raw metadata, calibrated iteration counts, executable/source hashes, all paired
samples and dispatch counts are retained in
[main results](benchmarks/write-scheduler-2026-10-11.json) and
[confirmation results](benchmarks/write-scheduler-confirmation-2026-10-11.json).
The comparison binaries and optimized build logs were originally written on the VM under
`target/write-scheduler-benchmark/20261011T164231168028Z/` and
`target/write-scheduler-benchmark/20261011T164431108037Z/`. The archived baseline
source, original baseline executable and its build log were under
`target/write-scheduler-benchmark/20261011T163037327297Z/`; the final runs record
and verify reuse of that baseline. These three build-cache directories were
absent when checked on October 17. The checked-in JSON retains samples,
hashes, and configuration evidence, not executable files or a complete build cache.
The full matrix runner preceded the compiler-reuse guard/runner-hash metadata
addition; the confirmation used the current runner. Calibration, sampling and
statistics were unchanged.

Correctness validation commands, counts and timings are in
[the Rust performance audit](RUST_PERFORMANCE.md#scheduler-validation).
Dependency construction still has quadratic worst-case work and storage; the
benchmark establishes removal of the cubic readiness scans for these workloads.
No public API or dispatch ordering change is required by this optimization.


## Baseline reuse fix and release verification

Baseline reuse was hardened by recording and comparing build settings, rejecting
unknown legacy metadata, and checking Cargo's per-target profiles. These guards
use explicit errors and also execute with `python3 -O`.

The normal release build and both versions in a fresh comparison reported these
Cargo profile levels. No Rust flags, Cargo profile overrides, custom workspace
profiles or Cargo configuration files were active. Scheduler fingerprint records
also contain no extra Rust flags. The runtime libraries and benchmark executable
therefore use optimization level **3**, matching [Cargo's release default](https://doc.rust-lang.org/cargo/reference/profiles.html#release).
Build-time tools can use Cargo's separate build-override settings.

| Target | Release `opt_level` |
|---|---:|
| `vfsi_core` | 3 |
| `vfsi_local` | 3 |
| `vfsi_posix` | 3 |
| `vfsi_sync` | 3 |
| `vnfs` | 3 |
| `write_schedule_benchmark` | 3 |

The normal build reused the ordinary cache using:

```sh
cargo build --release --locked --offline -p vnfs \
  --no-default-features --features posix,test-faults \
  --example write_schedule_benchmark --message-format=json -vv
```

The baseline and optimized executable hashes from the verified comparison exactly
match both original published datasets. The scheduler source and measured binaries
are unchanged; the earlier speedups remain measurements of these level-3 builds.
Fresh and matching-reuse checks each completed the two 256-request overlapping
sink/POSIX cases with five pairs and a 100 ms target. These focused checks do not
replace the original 35-case matrix.

`python3 scripts/test-ci-scripts.py BenchmarkBuildTests` passed 14 new tests;
`python3 scripts/test-ci-scripts.py` passed all 30 in 0.72 s reported test time
at the recorded revision; these are historical validation counts.
Coverage includes flags/profile changes, wrappers, configuration hierarchy and
includes, missing metadata, credential exclusion, Cargo artifact paths/profiles,
and checks under Python optimization. Real CLI verification completed:

| Selection | Result | Wall time |
|---|---|---:|
| reject-profile-opt-level | Rejected before builds | 0.10 s |
| reject-rustflags | Rejected before builds | 0.09 s |
| reject-rustflags-python-optimized | Rejected before builds | 0.37 s |
| reject-legacy-metadata | Rejected before builds | 0.09 s |
| accept-matching-settings | Accepted; two cases completed | 1.36 s |

`./scripts/test-ci-local.sh smoke` passed 167 Rust tests, with 11 fixture tests
ignored (1.98 s); `./scripts/test-ci-local.sh check` passed formatting
and Clippy (1.02 s). Python syntax and `git diff --check` were
checked. Tests and builds ran separately from timed benchmark samples. Runtime
code was unchanged for this P2 fix, so the earlier live NFS coverage was retained;
full CI and broader live/backend suites were not rerun.

[Release settings, executable hashes, raw comparison samples and validation records](benchmarks/write-scheduler-release-verification-2026-10-11.json)
are retained with the report. Normal release logs were written to
`/tmp/vnfs-release-verify.log`; CLI checks used `/tmp/vnfs-benchmark-build-*.log`.
Those temporary paths are not a durable archive.
