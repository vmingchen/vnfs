# Batched `du` traversal: development candidate versus frozen release

The seven-run candidate snapshot below predates the subsequent review fixes.
See [the review-fix recheck](du-review-fixes.md) for current identity tracking,
automatic build provenance and a bounded three-run performance recheck. The
archived source patches identify exactly which revision each experiment measured.

Measured November 1, 2026. The coordinated **vnfs 0.0.27** baseline, its
Cargo.lock and published results remain unchanged. This is a separately named
development experiment, not a release or a replacement for that baseline.

| Network | Kernel, ms | Frozen VFSI, ms | Candidate, ms | vs kernel | vs frozen VFSI | RPCs: kernel / frozen / candidate |
|---|---:|---:|---:|---:|---:|---|
| native | 1028.459 | 800.299 | 87.625 | 11.74× | 9.13× | 4,667 / 3,122 / 130 |
| added-1ms-rtt | 5776.566 | 3946.967 | 217.092 | 26.61× | 18.18× | 4,668 / 3,122 / 130 |

The same quiescent tree contains **1,555 directories and 12,440 128-byte files**,
branching six, depth four. Medians use seven capture-free invocations per path
and network, with rotated/reversed execution order. RPC medians use three
separate wire captures per path and network, counting attempts, including
rejected compounds. All **66 invocations** produced identical sorted output
(13,995 lines) and passed a separate parent-after-descendant output-order check.
Sibling output order is unspecified; byte-for-byte line order is not asserted.

Native means an isolated local veth with no added shaping. The controlled case
adds 500 µs in each direction, **1 ms added RTT**; ping medians were 0.068 ms
native and 1.240 ms delayed. Each command gets a fresh NFSv4.2 AUTH_SYS mount
with normal attribute caching and a warm server. Both release-built uutils
binaries use the same build profile and toolchain. This is not GNU `du`.
The existing Ganesha process, executable and configuration match the original
experiment. The kernel comparator uses the frozen binary with `VNFS_IMPL=off`.

## Changes and preserved behavior

- `vlistdirs` accepts page-level `ListDirControl::Prune(paths)`. Admission runs
  before descendant I/O and depth admission; names must be directory entries
  from that page. Pruned entries still consume the parent listing's budget.
  Ordinary `ControlFlow<()>` callbacks retain their existing control behavior.
- `du` uses recursive bounded pages instead of repeatedly calling the lazy
  event visitor. Approved siblings share compounds and retain parent-relative
  child cursors, removing repeated full-path resolution. No persistent pathname
  cache or additional thread pool is introduced.
- Root and descendant listing retain no-follow safety. Existing lazy
  enter/leave visitors are unchanged: an Enter callback can still prune before
  listing that directory. Their strict callback order is not used as permission
  to prefetch a sibling whose admission is unknown.
- `du` keeps one accumulator per directory and consumes ordinary files/pages
  immediately. Reverse reduction emits every directory after all descendants.
  Exclusions, hard-link deduplication, `--count-links`, `-S`, and file output
  retain their accounting behavior. Potentially linked files select their
  winner by the original DFS order, not the batched discovery order.

The tradeoff is retaining directory accumulators (and possibly linked-file
winners) until reduction, bounded by the existing traversal entry/path budgets.
Directory lines are emitted after the page traversal; ordinary file output is
streamed. This is not a filesystem snapshot or a constant-memory traversal.

## Wire evidence and remaining constraint

RPC attempts fall from **3,122 to 130**, a **95.8% reduction**. A representative
candidate capture contains **1,563 successful LOOKUP operations**, versus
9,027 in frozen VFSI, and exactly **1,555 successful READDIR operations**.
The candidate is still synchronous; the improvement comes from fewer, larger
compounds and reused traversal state, rather than overlapping RPCs.

Each of the six main candidate captures includes **three NFS4ERR_RESOURCE
responses**. The existing read-only fallback reduces its compound size and
completes. These rejected attempts are included in the 130 RPCs; successful
reply operations above exclude them. Server resource handling remains a tuning
constraint, not an omitted sample or a claim of zero protocol retries.
Every capture has zero capture drops, matched request/reply keys, no repeated
RPC XIDs and no retransmitted decoded RPC frames.

## Smaller-shape regression checks

The flat fixture contains one directory with 128 128-byte files:

| Network | Kernel, ms | Frozen VFSI, ms | Candidate, ms | vs kernel | vs frozen VFSI | RPCs: kernel / frozen / candidate |
|---|---:|---:|---:|---:|---:|---|
| native | 3.692 | 9.552 | 7.459 | 0.49× | 1.28× | 5 / 14 / 14 |
| added-1ms-rtt | 10.024 | 23.317 | 23.723 | 0.42× | 0.98× | 5 / 14 / 14 |

The narrow fixture is a depth-eight chain, nine directories with one 128-byte
file each:

| Network | Kernel, ms | Frozen VFSI, ms | Candidate, ms | vs kernel | vs frozen VFSI | RPCs: kernel / frozen / candidate |
|---|---:|---:|---:|---:|---:|---|
| native | 7.439 | 11.128 | 8.463 | 0.88× | 1.31× | 29 / 30 / 22 |
| added-1ms-rtt | 37.376 | 42.404 | 32.633 | 1.15× | 1.30× | 29 / 30 / 22 |

These use seven timing invocations and one captured invocation per path/network
(54 invocations per fixture). Output parity and post-order checks pass throughout.
The delayed flat case is 0.406 ms (1.7%) slower than frozen VFSI by medians,
with unchanged RPC counts. This small difference is not evidence of a large
regression or proof of zero overhead. The kernel remains faster on flat inputs;
the benefit depends on tree shape. The narrow chain improves over frozen VFSI
in both regimes. No confidence interval or dedicated-hardware claim is made.

## Verification and reproduction

- Full server-independent Rust suite: `./scripts/test-rust.sh`, passed.
- Required smoke checks and affected-target Clippy with warnings denied: passed.
- Live NFSv4.1 and NFSv4.2 regression: 64 admitted children in **10 RPCs**,
  positive client-scoped counters, excluded child never visited.
- Ten `du` unit tests pass, including post-order over multiple pages/cohorts,
  an unreadable excluded subtree, hard links, `-S`, and count-links behavior.
- The cross-directory hard-link test fails with discovery-order attribution
  (subdirectory size zero instead of five), and passes with DFS attribution.
- Ten benchmark tooling tests pass, including rejection of parent-before-child
  output. This task did not run live SMB or the complete coreutils application
  suite; native paging contracts, shared backend tests and adapter builds ran
  through the server-independent suite.

[Main results and raw captures](benchmarks/du-batched-2026-11-01/main/results.json),
[flat results](benchmarks/du-batched-2026-11-01/flat/results.json), and
[chain results](benchmarks/du-batched-2026-11-01/chain/results.json) retain every
sample. Each directory contains source patches, the candidate lock, binary
hashes, network calibration, NFS reply-status accounting, raw outputs and
original pcaps. The main directory also retains the exact measurement scripts;
[validation logs](benchmarks/du-batched-2026-11-01/validation/) preserve the tests
and candidate build time. Earlier profiling/preliminary runs are not used in
these medians. All **174 final invocations** passed verification.

Rebuild the explicit candidate with `cargo build --release --locked -p uu_du
--features vnfs`, then use `benchmarks/du/compare.py` as documented in the
[benchmark README](../benchmarks/du/README.md). Verify the retained evidence and
regenerate any of the three tables:

```sh
python3 benchmarks/du/report.py docs/benchmarks/du-batched-2026-11-01/main/results.json
```

The port candidate is on `perf/du-batched-traversal` at
`/home/ubuntu/vfsi-du-candidate`, with explicit temporary workspace dependency
patches. The published port and frozen checkout are untouched. No package
versions were changed and nothing was pushed or published for this experiment.
