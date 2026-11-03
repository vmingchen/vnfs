# Prunable traversal release candidate — November 2, 2026

The November 2 plan is exercised without an elapsed week-long freeze. The public
`Vfsi::vlistdirs`, `ListDirControl` and option types remain unchanged. The new
coverage tests the existing contract rather than introducing another traversal
API.

## Directory-cohort tuning

NFS admits at most **24 sibling directories** per speculative cohort. Anchored
child listing uses four operations per child plus SEQUENCE, so even after the
shape's adaptive window grows a full cohort uses at most 97 operations. The
negotiated request-byte, response-byte and operation ceilings still apply, and
RESOURCE fallback remains active for smaller or transient server limits. This
is a local batching policy, not a new hard-coded protocol maximum or a global
permanent RESOURCE ceiling. It deliberately trades maximum packing for a bounded
cohort: servers that reliably accept larger compounds can be underfilled. The
measurements below establish the Ganesha result, not a cross-server optimum.

The previous reviewed captures had three RESOURCE replies, all tagged
`readdir_children`. With the smaller cohort the same large-tree fixture takes
**81 wire RPCs instead of 130**, with no nonzero NFS replies in either capture.
Output parity and post-order accounting pass in every paired run.

| Network | Kernel median | Frozen vnfs 0.0.27 median | Development median | Wire RPCs: kernel / frozen / development |
|---|---:|---:|---:|---:|
| Native | 1,031.928 ms | 821.593 ms | 65.215 ms | 4,667 / 3,122 / **81** |
| +1 ms RTT | 5,685.344 ms | 3,917.096 ms | 155.766 ms | 4,668 / 3,122 / **81** |

The cohort candidate is 15.82× and 36.50× faster than kernel `du` in this run.
These are workload-specific, three-pair medians with one separate capture pair
per network, not a universal performance claim. The 130-RPC reference is the
previous reviewed release capture; its wall times are not mixed into this table.
The table's frozen vnfs 0.0.27 binary, source/lock checks and measurement method
are unchanged. See [large-tree raw evidence](benchmarks/du-cohort-2026-11-02/results.json)
and [decoded error checks](benchmarks/du-cohort-2026-11-02/directory-statuses.json).

Smaller shapes remain explicit regression checks:

| Shape | Network | Kernel / frozen / development median, ms | RPCs: kernel / frozen / development |
|---|---|---:|---:|
| Flat: 1 directory, 128 files | Native | 3.722 / 7.128 / 7.701 | 5 / 14 / 14 |
| Flat | +1 ms RTT | 9.842 / 21.285 / 22.295 | 5 / 14 / 14 |
| Chain: 9 directories, 9 files | Native | 9.963 / 11.274 / 8.925 | 29 / 30 / 22 |
| Chain | +1 ms RTT | 38.553 / 41.776 / 30.879 | 29 / 30 / 22 |

The flat case is still slower than kernel traversal. Compared with the older
frozen binary its candidate time differs by 0.573 ms native and 1.010 ms delayed;
these small samples do not establish timing equivalence. A single-directory
walk does not hit the sibling cap and its RPC count remains unchanged. The
chain retains its prior 22-RPC shape. [Flat](benchmarks/du-cohort-2026-11-02-flat/results.json)
and [chain](benchmarks/du-cohort-2026-11-02-chain/results.json) evidence includes
all samples and captures.

## Conformance and release gates

A shared test fixture spans directory-page boundaries with 270 child directories.
Dummy/local, POSIX, Auto and direct NFS test the same admission, global entry and
path-byte budgets, depth-zero pruning, stop/error and unlocked callback
assertions. Shallow roots may interleave differently by backend: a budget error
must identify the actual failing root, not an assumed page order.

Live Auto checks use both NFSv4.1 and v4.2 mounts, and verify direct routing.
Their fixtures are prepared through the kernel namespace before read-only Auto
traversal. They deliberately do not assume kernel/direct-client cache coherence:
immediate nested creation through Auto can hit stale kernel namespace lookups,
which is a separate limitation from traversal admission. Existing renamed-child
and swapped-ancestor tests remain part of the selected NFS and local suites.
Deterministic initial-connection and page-boundary faults exercise a real
reconnect before callbacks, including a pruned recursive walk. A failure after
its first delivered page must return the transport error without replaying the
admission callback. These are backend-boundary injection tests, not malformed
on-wire RPC decoder tests.

Samba runs shared shallow paging, multi-root budget, re-entry, stop and callback
error checks. Its recursive no-follow contract remains explicitly unsupported:
the test requires an `Unsupported` error before callbacks. It does not claim
recursive SMB pruning coverage by silently falling back.

The four-port workflow now triggers on relevant source, SDK, registry and check
changes in main pushes and pull requests, in addition to schedule and dispatch.
It also runs a frozen ABI-v3 client (without today's header) and optional-symbol
loader cases. Mirror promotion checks the latest eligible **same-commit main**
CI and port-workflow runs, and refuses missing, pending, failed, wrong-SHA or
wrong-branch results before installing the mirror key. A newer pending/failed
run cannot reuse an older successful run. For docs-only commits, dispatch the
port workflow explicitly before promotion.

The [rsync benchmark](rsync-benchmark.md) freezes public vnfs 0.0.28/vfsi-c 0.3.12
and records the slower released sender path alongside its parity and fallback
checks. No additional port or package release is introduced by this candidate.

## Validation status

The local smoke runner, full server-independent Rust runner (including detached
Python extension workspaces), strict Clippy and C ABI checks pass. Live native
NFSv4.1/v4.2 each run all 36 backend tests; the public NFS targets each pass 136
tests with two fixture-dependent ignored tests. The new ignored Auto pruning
target was explicitly exercised on both protocol versions, rather than counted
as coverage while skipped. Samba's shared shallow checks pass with its recursive
safety limitation asserted. CI-script and benchmark-harness tests also pass.

All four registered port sources pass the local equivalent of the exact matrix.
Rsync's new documentation-only head is tested from local immutable Git objects
until it is pushed; the benchmark remains pinned to its unchanged parent runtime
source. Findutils' unrelated working-tree edits were excluded and preserved.
[Validation summary](benchmarks/traversal-conformance-2026-11-02.json) records
revisions, commands, timings and limitations. Git's compatibility script now
isolates the SDK's `CARGO_TARGET_DIR` so Git's own Rust archive is linked from
its Makefile's expected checkout-local path.

GitHub same-commit CI, the automatically triggered four-port gate and mirror
promotion still require pushing this candidate and the rsync documentation head.
Local success is not reported as remote CI success. No package versions or
published artifacts are changed by this work.
