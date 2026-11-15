# Read optimization follow-up

This repeats the integrated comparison after two reviewed read optimizations.
Baseline remains 70bea6a; the final production source is 91d9798 plus the
[archived source patch](source-read-optimization-2026-11-14/final-source.patch).
The unchanged benchmark sources, lockfile, software-profiler summaries and hashes
are archived beside that patch. [All raw measurements](results-read-optimization-2026-11-14.json)
include 27 scenarios × nine randomized pairs, separate allocation controls and
exact syscall/NFS/SMB wire controls. The [earlier report](README.md) is retained.

## Changes and interpretation

- Dense local owned reads allocate preflight index maps and placeholder outcomes
  only after a rejection. Valid inputs still enter one descriptor batch. Sparse
  inputs, malformed result counts, zero-length reads, cursor updates and prefixes
  retain their existing semantics.
- Single-chunk SMB reads move the decoded response payload to the caller instead
  of allocating another buffer and copying it. Multi-chunk reads and confirmed
  prefixes after failures retain the original accumulation path.
- Software CPU profiling of 200,000 healthy POSIX batches located additional
  self time in PosixIo::read_owned (approximately 0.45% baseline versus 3.57%
  before optimization). Hardware cycle events were unavailable. These sampled
  percentages identify a hot region, not a precise causal latency decomposition.
- Forcing a fresh POSIX result allocation was tested and discarded: it added
  requested bytes and did not show a clear timing improvement. No padding, unsafe
  vector conversion, backend serialization or broader public interface was added.
- Healthy allocation-call counts now meet the measured gate. POSIX and io_uring
  save one call per 64-input cohort; SMB saves 63 net calls. NFS remains equal.
  All five healthy controls have lower peak-live Rust memory and zero retained delta.
  These are the 4 KiB retained-handle fixture results, not a promise for every
  read shape, allocator or platform.
- Healthy POSIX still has a roughly 3% paired median cost in this run; it has
  **not** been proven eliminated. The full-run interval is wide due to occasional
  near-2× wall-time outliers in both variants. The separate diagnostic pairs are
  retained in the raw data rather than substituted for the full run.
- Native and opaque failure boxing/sharing costs remain visible. Timing parity
  is not claimed. No extra healthy I/O, RPC/request bytes, io_uring SQEs/CQEs or
  submission waves occur; no retained ring buffers/descriptors remain.

## Healthy allocation controls

Counts and bytes are per 64-input cohort; peak is maximum extra live Rust bytes.
The observer excludes C/library heaps. Wire controls are independent of timing.

| Backend | Calls old → new | Requested bytes old → new | Peak bytes old → new |
| --- | ---: | ---: | ---: |
| posix | 72 → 71 | 280576 → 281592 | 280576 → 276992 |
| uring | 74 → 73 | 286208 → 285688 | 286208 → 276992 |
| nfs41 | 289 → 289 | 358416 → 356872 | 305829 → 300117 |
| nfs42 | 289 → 289 | 358416 → 356872 | 305829 → 300117 |
| smb | 3141 → 3078 | 1.88218e+06 → 1.62463e+06 | 287272 → 283176 |

## Uninstrumented timings

Microseconds per cohort. Ratios are medians of paired ratios with deterministic
bootstrap 95% intervals; they are not quotients of separately computed medians.

| Scenario | Baseline µs | Standard µs | Paired ratio [95% interval] |
| --- | ---: | ---: | ---: |
| nfs41/all | 8019.852 | 7829.463 | 0.976 [0.887, 1.019] |
| nfs41/healthy | 437.358 | 437.478 | 0.993 [0.952, 1.016] |
| nfs41/lossy | 563.976 | 568.250 | 1.010 [0.971, 1.106] |
| nfs41/mixed | 556.414 | 554.352 | 0.973 [0.856, 1.028] |
| nfs41/void | 1.009 | 0.975 | 0.968 [0.959, 1.010] |
| nfs42/all | 7532.581 | 8059.220 | 1.046 [1.010, 1.190] |
| nfs42/healthy | 437.988 | 447.627 | 1.015 [0.998, 1.048] |
| nfs42/lossy | 570.906 | 553.528 | 0.986 [0.911, 1.007] |
| nfs42/mixed | 547.364 | 561.839 | 1.025 [0.999, 1.087] |
| nfs42/void | 1.022 | 0.983 | 0.970 [0.929, 1.000] |
| posix/all | 24.649 | 23.185 | 0.946 [0.931, 0.973] |
| posix/fanout_nfs | 0.488 | 3.338 | 6.841 [6.777, 6.955] |
| posix/fanout_opaque | 1.878 | 3.233 | 1.719 [1.705, 1.732] |
| posix/healthy | 30.791 | 30.671 | 1.030 [0.973, 1.855] |
| posix/lossy | 29.491 | 30.522 | 1.035 [1.026, 1.076] |
| posix/mixed | 29.142 | 30.880 | 1.039 [1.028, 1.064] |
| posix/void | 10.998 | 10.983 | 0.999 [0.992, 1.028] |
| smb/all | 2992.116 | 3091.058 | 1.032 [0.991, 1.093] |
| smb/healthy | 15285.156 | 15303.156 | 0.848 [0.751, 1.031] |
| smb/lossy | 18726.005 | 18461.121 | 0.998 [0.883, 1.123] |
| smb/mixed | 19193.355 | 14150.674 | 0.738 [0.671, 1.102] |
| smb/void | 4990.282 | 4929.950 | 1.002 [0.924, 1.143] |
| uring/all | 20.202 | 22.053 | 1.095 [1.086, 1.115] |
| uring/healthy | 37.180 | 38.592 | 1.009 [0.988, 1.072] |
| uring/lossy | 36.788 | 38.148 | 1.040 [0.990, 1.057] |
| uring/mixed | 36.890 | 36.883 | 0.999 [0.945, 1.060] |
| uring/void | 13.913 | 13.703 | 0.994 [0.976, 1.001] |

## Verification and limits

The new local test exercises 60 dense/sparse/all-rejected, positional/current,
zero/nonzero and correct/missing/extra-result combinations. The required live
SMB test checks zero, short, EOF, exact chunk and multi-chunk boundaries. Existing
failure-prefix, recovery and cleanup tests remain intact. Independent code reviews
found no actionable problems. Broader final check results are recorded in the
[migration progress](../../vferror-migration-progress.md).

The migration removes the measured extra healthy allocation; remaining POSIX
latency is an investigated representation/dispatch cost, not an unmeasured
exception or a zero-regression claim. The design requires investigating repeatable
timing regressions and reporting costs; it does not specify zero timing overhead.
No publication, active port repin or mirror promotion is authorized by these
local measurements. Optional SMB restart/DFS and non-Linux runtime coverage
remain omitted. Earlier source archives and test coverage are unchanged.
