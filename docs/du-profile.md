# Why the frozen `du` baseline is 1.43× faster at +1 ms RTT

Profiled November 1, 2026. This examines the unchanged
[vnfs 0.0.27 benchmark](du-benchmark.md), not a new implementation or baseline.
The binary SHA-256 and fixture identity match the published experiment.

The main limit is **serialized per-directory RPCs**, not file-data copying.
The kernel path needs approximately three RPCs per directory; VFSI needs two.
Neither path overlaps its RPCs in this experiment. At sufficiently high latency,
that call-count ratio approaches 1.50×, so the measured 1.43× is unsurprising.
This is a limit of the observed traversal, not an inherent limit of VFSI.

| Evidence | Kernel | VFSI |
|---|---:|---:|
| RPCs at +1 ms added RTT, median of three captures | 4,669 | 3,122 |
| READDIR calls per capture | 1,555 | 1,555 |
| Separate path-resolution calls per capture | — | 1,555 |
| Maximum observed outstanding RPCs, all six captures per method | 1 | 1 |
| Native median wall time, original seven runs | 1,150.498 ms | 948.190 ms |
| Delayed median wall time, original seven runs | 5,872.339 ms | 4,101.584 ms |
| Increase from adding 1 ms RTT | 4,721.841 ms | 3,153.394 ms |

The increases are close to one extra millisecond per call. For VFSI, the
original uninstrumented timing runs report a median **4,033.8 ms inside
synchronous RPC calls**, versus 4,101.584 ms wall time. The median per-run
fraction is **98.32%**, compared with 93.65% at native latency. This RPC timer
includes transport waits, server work, network delay and RPC processing; it is
not a CPU-time measurement or a pure network-delay measurement. Some CPU work
and printing on other threads overlap it.

## 1. Post-order presentation bypasses recursive sibling batching

The port calls `listdir(...with_enter_leave(true), ...)` to accumulate child
usage before emitting each directory. In the frozen implementation:

1. `vfsi-core/src/api/listdir.rs` selects `buffered()` for enter/leave events.
2. `walk_events()` invokes its listing closure once per entered directory.
3. That closure calls `read_dir_with_options(path, ...recursive(false))`.
4. The scalar collector calls `read_dirs_with_options(&[path], ...)`: a vector
   with **one** directory. It no longer retains the recursive engine's child
   cursor when it later descends into a listed subdirectory.
5. The NFS backend resolves that fresh pathname, then sends READDIR separately.

This is corroborated by every VFSI capture: **1,555 resolve compounds plus
1,555 READDIR compounds**, with only 12 other calls for setup, attributes and
kernel-side checks. Every READDIR compound is `SEQUENCE, PUTFH, READDIR`.
Each directory fits in one wire page; continuation pages do not explain the
call count. The port needs the current post-order accounting and pruning
semantics; simply disabling enter/leave events would change its behavior.

The recursive native paging engine already supports bounded sibling cohorts
and anchored child cursors. Those facilities are not used for descent by this
buffered event walker. The first optimization experiment should address this
execution-policy boundary while retaining post-order output, pruning before
descent, cancellation, aggregate budgets and no-follow safety.

## 2. Repeated full-path lookup and small compounds

The wire contains **9,027 LOOKUP operations** in a representative VFSI run,
including setup. The 1,555 directory-resolution compounds account for 9,020.
For the 1,296 deepest directories, each resolution repeats six LOOKUPs before
GETFH. Retaining parent-relative traversal state could avoid repeated ancestor
lookup; a plain pathname cache would require separate coherence/safety analysis.

VFSI telemetry reports 5.89 operations per compound on average, nine at most,
and roughly 167 encoded compound bytes per call. The captured CREATE_SESSION
reply permits **256 operations**, **4 MiB request/response messages**, and
**64 outstanding requests**. The byte telemetry excludes RPC authentication
and framing; it is not a full wire-message size. Nonetheless, these small
resolve/READDIR compounds are not being split by the negotiated operation cap.
All captures have successful NFS statuses and no repeated RPC XIDs; RESOURCE
fallback or failed attempts are not the explanation.

Combining resolution and READDIR could remove a dependency round trip per
directory. Batching anchored siblings can reduce calls further. These are
proposed experiments, not measured speedups. Increasing negotiated limits
alone cannot help a planner receiving one directory at a time.

## 3. No latency hiding

Matching every CALL to its reply by connection port and XID gives a maximum of
**one outstanding RPC** across all 12 captures, for both methods. VFSI's mean
request-start interval in the first captured sample rises from 0.332 ms native
to 1.371 ms with added delay. The kernel's rises from 0.269 to 1.307 ms.

The capture-point CALL-to-reply interval is not the complete application RTT:
netem delays and packet taps can fall on different sides of the observation
point. In particular, the roughly 0.6 ms delayed capture intervals must not be
interpreted as evidence that the configured 1 ms added RTT was absent. Use the
calibrated ping RTT, full request cycles and original wall-time changes.

After restoring useful compounds, evaluate bounded multi-request overlap if
latency still dominates. The session's concurrency allowance alone does not
make a synchronous caller or its shared backend lock concurrent.

## 4. Secondary CPU and syscall costs

Separate diagnostic runs used `perf stat`, software `cpu-clock` sampling at
499 Hz with DWARF stacks, `/usr/bin/time -v`, and `strace -f -c`. The frozen
binary was not rebuilt. Three counter runs per method/regime gave these
medians for the timed `du` process and its threads:

| Network | Method | User CPU | System CPU | Voluntary context switches | Peak RSS |
|---|---|---:|---:|---:|---:|
| native | kernel | 0.02 s | 0.33 s | 6,247 | 6,068 KiB |
| native | VFSI | 0.09 s | 0.39 s | 14,365 | 13,044 KiB |
| +1 ms RTT | kernel | 0.02 s | 0.28 s | 6,249 | 6,324 KiB |
| +1 ms RTT | VFSI | 0.11 s | 0.35 s | 14,321 | 8,928 KiB |

CPU profiles emphasize scheduler wakeups and transport synchronization.
`try_to_wake_up` accounts for about 31% of native and 37% of delayed VFSI
CPU self samples. Stacks include ntirpc condition signaling and the port's
per-entry `mpsc` printing channel. The profiles have only 176/147 VFSI samples,
so these percentages identify a direction, not a precise cost attribution.
Hardware cycles/instruction counters are unavailable on this VM.

The native syscall trace contains 3,120 `sendmsg`, 6,250 `recvfrom`, 3,122
`epoll_pwait` and 39,028 futex calls for VFSI, versus 11,756 futex calls for
the kernel path. These include small process-wrapper overhead. Futex failures
can be normal synchronization races; they are not filesystem errors. Counts
support investigating synchronization after reducing RPCs. Traced syscall
durations include sleeps, parent waits and overlapping threads, so their
percentages cannot be added into a wall-time breakdown.

Both paths also make about 14,000 writes for `-a` output. Printing and channel
handoffs are worth testing at native latency, but their shared output volume
and the delayed RPC timer argue against making them the first optimization.
Allocation, hashing and path construction appear in the CPU samples without
dominating them. Counter runs have zero major page faults and small RSS;
there is no evidence of memory-pressure bottlenecks. There are no NFS READ
operations, so file-data zero-copy is not relevant to this traversal.

## Evidence and limits

The [profile summary](benchmarks/du-profile-2026-11-01/summary.json),
[wire analysis](benchmarks/du-profile-2026-11-01/wire-analysis.json), and
[diagnostic run metadata](benchmarks/du-profile-2026-11-01/cpu-results.json)
retain the measurements. Adjacent text files contain counter, resource,
syscall and flat CPU reports. Original pcaps remain in the frozen benchmark
record; full perf data, stacks, output files and exact collection/analysis
scripts are retained on the lab VM at
`/home/ubuntu/vfsi-du-profile-20261101`.

All **20 diagnostic invocations** matched the frozen output digest and 13,995
lines; fixture identity matched before each trial and after collection. The
analysis reconciles all 12 original captures with their recorded RPC counts.
Software sampling and especially strace perturb scheduling; none of these
instrumented wall times replaces the published timing table. No server CPU
profile was collected, so separate Ganesha CPU attribution remains open.
This is still one shared VM and the same quiescent directory-heavy fixture.
No application or library optimization was made.
