# Integrated standard-error measurements

Status: local measurements and work-parity controls completed. Acceptance of
the measured healthy POSIX/SMB costs remains open; these are not publication
approval or a claim of universal performance parity.

This compares actual Vfsi clients with the same independent VecResult semantics:
baseline `70bea6a` versus `f68f5ec` plus the archived
[final source delta](source-2026-11-14/final-source.patch), which makes NFS
descriptor failure storage lazy. The latter removes a redundant healthy
allocation without changing dispatch, status, progress or cleanup. The recorded
source and binary hashes identify exactly what ran.

[Raw samples and controls](results-2026-11-14.json) contain 27 scenarios, nine
randomized baseline/final pairs each, allocation controls, native work counters,
syscall summaries, SMB wire counts and the rejected optimization experiment.
The actual compiler is Rust 1.93.1 on Linux aarch64; release builds use the
existing profile/target cache, no new build flags or cache cleaning.

## Findings

- Healthy io_uring and both NFS minor versions are near parity in this local
  sample. SMB timing is noisy; its wide interval cannot establish a small change.
- Healthy POSIX shows a repeatable roughly 6% slowdown in the final run
  (paired median interval roughly 3–11%). The first independent run showed
  about 3%. This remains a measured cost, not a resolved regression.
- Healthy POSIX, ring and NFS allocation-call counts match baseline. NFS lazy
  failure slots offset one vector-relayout allocation caused by smaller errors.
  SMB retains one extra allocation call per 64-file descriptor-read batch.
- A stack probe located that call in standard Vec in-place result conversion:
  72-byte native result slots become 40-byte owned slots, requiring an 8-byte
  allocation shrink. The resulting allocation request is 4,600 bytes. Avoiding
  it by restoring padding or unsafe vector conversion was not adopted.
- Bare errno/category causes remain allocation-free. Protocol and opaque/group
  failures pay standard-error boxing/sharing costs. Native fan-out and all-ring
  failure timing regressions below are explicit costs of the selected ownership.
- No new I/O, RPC calls, serialization/submission waves or replay was observed.
  Every benchmark validates successful lengths/offsets, boundary bytes and failure state/native
  category/status. Lossy helpers additionally validate selected index/status/
  transport. Every client run explicitly closes all 64 opened handles.

The standard cause is 8 bytes and OpError is 24 bytes on this target, versus
32 and 48 bytes for the indexed baseline. These are observations, not Rust ABI
promises. Smaller causes do not guarantee fewer allocation calls or faster
healthy execution after enum/vector layout changes.

## Uninstrumented timing

Times are median microseconds per 64-input operation. The ratio is the median
of paired final/baseline ratios with a deterministic bootstrap 95% interval,
not the quotient of the separately computed medians. Only uninstrumented
binaries contribute timing. Void NFS validates already-stable descriptors and
issues no COMMIT; it is not a storage-flush latency comparison.

| Scenario | Baseline µs | Standard µs | Paired ratio [95% interval] |
| --- | ---: | ---: | ---: |
| nfs41/all | 7565.367 | 7715.287 | 1.034 [0.984, 1.183] |
| nfs41/healthy | 439.496 | 439.725 | 1.011 [0.935, 1.021] |
| nfs41/lossy | 558.309 | 569.235 | 0.999 [0.962, 1.084] |
| nfs41/mixed | 552.650 | 560.994 | 1.017 [1.006, 1.097] |
| nfs41/void | 0.984 | 0.981 | 0.977 [0.972, 1.007] |
| nfs42/all | 7685.900 | 7946.647 | 1.013 [0.962, 1.050] |
| nfs42/healthy | 446.175 | 442.143 | 1.008 [0.978, 1.065] |
| nfs42/lossy | 598.326 | 584.773 | 0.931 [0.788, 1.039] |
| nfs42/mixed | 552.354 | 556.697 | 1.005 [0.982, 1.046] |
| nfs42/void | 0.979 | 0.975 | 0.993 [0.959, 1.017] |
| posix/all | 24.527 | 22.905 | 0.934 [0.896, 0.950] |
| posix/fanout_nfs | 0.490 | 3.312 | 6.812 [6.559, 6.829] |
| posix/fanout_opaque | 1.849 | 3.266 | 1.771 [1.753, 1.789] |
| posix/healthy | 29.391 | 30.834 | 1.061 [1.031, 1.109] |
| posix/lossy | 29.484 | 30.594 | 1.037 [1.008, 1.054] |
| posix/mixed | 29.381 | 30.686 | 1.047 [1.031, 1.103] |
| posix/void | 10.988 | 10.927 | 0.990 [0.964, 0.998] |
| smb/all | 2986.676 | 2990.961 | 1.001 [0.958, 1.011] |
| smb/healthy | 14081.759 | 15366.388 | 0.981 [0.669, 1.667] |
| smb/lossy | 15543.576 | 16125.961 | 0.881 [0.783, 1.709] |
| smb/mixed | 13655.131 | 14757.730 | 0.860 [0.611, 1.567] |
| smb/void | 4923.017 | 4962.676 | 0.995 [0.899, 1.144] |
| uring/all | 20.299 | 22.016 | 1.091 [1.071, 1.138] |
| uring/healthy | 36.782 | 36.697 | 0.997 [0.992, 1.029] |
| uring/lossy | 36.743 | 36.634 | 1.000 [0.981, 1.022] |
| uring/mixed | 36.715 | 37.751 | 1.031 [1.000, 1.057] |
| uring/void | 13.819 | 13.651 | 0.988 [0.985, 0.998] |

## Healthy Rust allocations

Calls/bytes are per 64-file batch; peak is additional simultaneously live Rust
allocator bytes above the warmed client. C/libntirpc heaps are not measured by
this allocator. Runtime background work can cross the sampling boundary:
separate SMB repeats report its small scheduling variation and zero retained
delta in all seven final repeats. Occasional main controls retain a still-live
transport task at the measurement boundary; this is not a post-shutdown leak
measurement. Closed-handle and original opaque-source release checks are separate.

| Backend | Calls baseline → standard | Requested bytes baseline → standard | Peak extra live bytes baseline → standard |
| --- | ---: | ---: | ---: |
| posix | 72 → 72 | 280576 → 282104 | 280576 → 277504 |
| uring | 74 → 74 | 286208 → 286200 | 286208 → 281600 |
| nfs41 | 289 → 289 | 358416 → 356872 | 305829 → 300117 |
| nfs42 | 289 → 289 | 358416 → 356872 | 305829 → 300117 |
| smb | 3141 → 3142 | 1.88218e+06 → 1.88678e+06 | 287272 → 287272 |

## Work controls and limits

The traced controls execute separately from timing. POSIX syscall and real ring
submission/completion/wave counts match. All ring timing/control rows submit
64 SQEs and receive 64 CQEs per nonempty batch in one wave, without retained
buffers/descriptors. Both NFS versions match RPC/compound/operation counts and,
in the separate VNFS_STATS control, encoded request bytes.

SMB packet capture requires zero kernel drops, expected OPEN/READ/CLOSE counts,
and completed tree disconnect. Primary request and terminal response command
counts match, including the failure-heavy related compounds. STATUS_PENDING
interim replies vary with server scheduling; their counts are recorded separately
and are not additional logical requests. Capture buffering/delivery controls
affect only the observer, not the benchmark or host network configuration.

Live NFS uncertainty/short-prefix controls, current/old/missing C ABI consumers
and package tests are recorded in the [migration progress](../../vferror-migration-progress.md).
Optional SMB restart/DFS controls and non-Linux execution were not performed.
No total-process heap, cold-storage, added-latency or broad cross-machine claim
is made.

## Rejected conversion experiment

Exact final vector capacity reduced requested bytes with unchanged allocation
calls and healthy/mixed/failure-heavy peak in initial controls. A required
zero-byte control exposed 1,296 additional transient bytes for SMB, so the
production change and its experimental test were dropped together. The original
test coverage remains. Its source diff and raw controls are in the result JSON.

An initial old-code regression control incorrectly reused a newer shared Cargo
artifact because archive timestamps were older. Refreshing only owned source
snapshot timestamps forced a rebuild: the old release test then failed at
capacity 5 versus expected 3, while the candidate passed. Final comparison
snapshots were fully rebuilt with refreshed inputs and verbose compiler logs;
no cache was cleared or reconfigured and profiles/toolchains were unchanged.

## Reproduce

Use an owned Linux workspace with the existing local NFSv4.1/v4.2 fixture at
127.0.0.1:2049 and guest SMB share vfsi-test. Never use or change the stale host
NFS mount. Snapshot the two revisions with git archive into distinct owned
directories. Apply final-source.patch only to the final archive. Copy the frozen
Cargo.toml, Cargo.lock and src/main.rs below benchmarks/error-integration in both.
Refresh archive input timestamps before each source build when sharing a Cargo
target directory; record verbose build logs so timestamp-cache reuse cannot
masquerade as a control.

Current sources require `--features standard-error`; omitting that feature is
intentional only in the old indexed baseline. Build each with
`cargo build -vv --release --locked --manifest-path .../Cargo.toml`, adding
standard-error for final. Copy the resulting executable immediately to
WORK/baseline-timing or WORK/final-timing. Rebuild with allocation-stats added
for the corresponding WORK/*-allocations binaries; allocation-profile is a
separate optional size-histogram diagnostic, never a timing binary.

WORK/baseline.revision and final.revision contain the full source commit IDs;
WORK/final-source.patch is the exact applied delta. Create WORK/posix with 64
file-N files, each 4,096 copies of byte N (N=0..63). For each network backend,
choose a fresh private /prefix, run the final binary with
`BACKEND prepare 1 /prefix` (BACKEND nfs41, nfs42 or smb), and record that
prefix in WORK/BACKEND.root. Do not overwrite an existing namespace.

Run the frozen tools with the same compiler environment:

```sh
python3 run.py --work WORK --output WORK/results.json --pairs 9 --cpu 2
python3 trace.py --work WORK
python3 -m unittest discover -s . -p 'test_*.py'
```

trace.py needs strace, tcpdump, tshark and permission to observe only the owned
loopback fixtures. Sources, manifests and data in source-2026-11-14 have SHA256
entries; binary hashes and exact fixture locations are in the raw result.
Do not treat a skipped fixture, empty test target or traced duration as timing
or runtime coverage. No push, publication, active port change or mirror
promotion occurred.
