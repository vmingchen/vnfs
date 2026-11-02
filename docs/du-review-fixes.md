# Post-review `du` fixes and regression check

The three review findings are fixed without changing the frozen coordinated
vnfs 0.0.27 release or its benchmark record.

- Ordinary files now keep a lightweight seen-identity set. A move between
  directory pages cannot count the same inode twice, even when `nlink` stays
  one. This set and deferred hard-link winners have mutually exclusive
  ownership; transitions `1→2` and `2→1` also count once. `--count-links`
  continues to count every observation. File contents/output are not buffered
  by this set; its size is bounded by the traversal entry budget.
- The comparison runner invokes a locked release build of the candidate before
  recording its source and binary provenance. It takes the executable path
  from Cargo's JSON artifact messages, including custom target directories.
  Build failures and absent artifacts abort instead of using an old binary.
- Comparison reports enforce the same minimum as release reports: three
  timing samples and at least one capture per variant/network. Short pilot
  collections cannot be rendered as publishable speedup tables.

## Regression coverage

The new identity test uses real before/after POSIX metadata delivered in
separate pages, avoiding timing-dependent races. It reproduced a five-byte
file being counted as ten bytes. It covers moves with `nlink=1`, both link-count
transitions, and both count-links settings. Aggregation is separated from the
transport call so the test exercises the actual accumulator and output path.
Existing DFS hard-link attribution, post-order and pruning tests still pass.

The benchmark tests replace a stale executable with the build artifact before
source snapshotting, verify custom artifact paths, reject build failures and
missing artifacts, exercise every trial in a full mocked comparison, and
reject undersampled variants or absent captures. All **11 du tests** and
**15 benchmark-tool tests** pass. Strict application Clippy and the repository
smoke suite pass. The unchanged Rust backend suites were not rerun in full;
they passed during the preceding batching implementation. Nothing was pushed.

## Bounded performance recheck

Same 1,555-directory / 12,440-file fixture and fresh-client/warm-server controls
as the [earlier experiment](du-batching.md). This is a separate collection:
**three capture-free timing invocations and one captured invocation per path
and network**, 30 invocations total. Every output matched and every directory
followed its descendants. The runner rebuilt/checked the actual candidate and
records its build command, toolchain, selected executable and binary hash.

| Network | Kernel, ms | Frozen VFSI, ms | Candidate, ms | vs kernel | vs frozen VFSI | RPCs: kernel / frozen / candidate |
|---|---:|---:|---:|---:|---:|---|
| native | 1196.828 | 990.976 | 110.164 | 10.86× | 9.00× | 4,667 / 3,122 / 130 |
| added-1ms-rtt | 6027.557 | 4124.566 | 234.561 | 25.70× | 17.58× | 4,669 / 3,122 / 130 |

RPC counts remain 130 for the candidate. The speedups relative to frozen VFSI
remain close to the earlier 9.13×/18.18× result. Absolute times differ on this
shared VM; three samples do not establish statistical equivalence or replace
the archived seven-run result. This is a regression check, not a new package
release or a change to the public frozen baseline.

[Raw results](benchmarks/du-review-fixes-2026-11-01/results.json) retain source
patches, lock, build logs/messages, raw output and pcaps. The adjacent validation
directory preserves expected failures before the fixes and successful checks
afterwards. The first runner startup exposed a missing import before any trial;
it was fixed and covered by the full trial-loop test. Only the completed
collection above contributes timings.

```sh
python3 benchmarks/du/report.py docs/benchmarks/du-review-fixes-2026-11-01/results.json
```

Changes remain uncommitted in `~/vnfs` and the coreutils candidate worktree
`~/vfsi-du-candidate` (`perf/du-batched-traversal`).
