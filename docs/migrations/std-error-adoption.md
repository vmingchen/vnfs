# Standard I/O error adoption (local verification)

This is a source-breaking Rust API change after the VecResult migration.
No publication, active port repin, push, PR or mirror promotion is authorized.
The C ABI remains v3 with existing optional-symbol compatibility.

## Exact disposable port sources

| Port | Source revision |
| --- | --- |
| coreutils | `bc4383aa7272e877613fc0f2cce4c9fc4de06318` |
| findutils | `76aa4345c42cd9f70ccba3200a6b58339eef4a26` |
| git | `b76079cec0d5a0988cd3882e1784ca0665f6df6a` |
| rsync | `11630dad78294eedb23a76d92ba91cd32c77cb1c` |

The four active checkouts under ~/gh were not modified. Each check ran in an
owned snapshot of its recorded HEAD. The Rust snapshots temporarily resolved
vnfs from /home/ubuntu/vnfs02/crates/vnfs; resolved path dependencies and disposable
lockfiles were checked. Their existing published pins remain unchanged.

[coreutils adoption patch](std-error-coreutils.patch) and
[findutils adoption patch](std-error-findutils.patch) contain source adaptations
only, excluding temporary dependency paths, lockfile updates and build outputs.
Apply a patch in a fresh matching port snapshot with
`git apply --unidiff-zero /path/to/std-error-PORT.patch`; retarget its one inline
vnfs dependency to the local source for verification. No released version is
invented here.

Coreutils Session methods keep their established io::Result application boundary
and explicitly discard IndexedError attribution with into_cause. Native COPY
fallback still requires NotAttempted; Failed/Unknown never permit replay.
Selected COPY diagnostics retain their original source and path through
IndexedError before explicit scalar extraction. Find's pruning, scheduling,
unreadable-child reporting and side-effect handling are unchanged.

## Executed compatibility checks

- Coreutils: cp 16, du 12, ls 31, and uucore 381 unit tests pass; one pre-existing
  uucore control is ignored. rm/rmdir compile, the five-utility multicall builds,
  and both round-robin/no-clobber/destination-symlink CLI regressions pass.
- Findutils: all 286 library tests pass. Real CLI controls establish dummy
  activation, metadata/predicate parity, pruning, early stop, unsupported-mode
  fallback, unreadable-child/readable-sibling handling and matcher effects.
  Tests ran as the ordinary user in an owned private mount namespace to exclude
  the host's stale /mnt/nfs mount; host mounts and services were unchanged.
- Git: the real USE_VFSI build and C consumer suite pass: zero/tiny/normal
  prefetch budgets, initialization failure, bad ABI/index/partial replies,
  missing optional symbols, alternate object directories, special files,
  concurrent freshening, retained cursors and late fallback.
- Rsync: its real --enable-vfsi build passes incremental/nonincremental parity
  and all 11 fixture/fallback tests.
- Independently compiled C ABI-v3 and optional-outcome consumers pass against
  the rebuilt SDK, real published vfsi-c 0.3.11, missing-library/POSIX fallbacks,
  and required NFSv4.1/v4.2 lost/uncached/short/prefix/later-wave controls.
- Detached Python native/package tests and full workspace gates are recorded in
  the [migration progress](../vferror-migration-progress.md). Fixture-dependent
  skips and zero-test thin extension targets are not runtime coverage.

These are local compatibility checks, not public CI or release qualification.
Read-only review covers the port adaptations and error guide; substantive stale
custom-error migration instructions were corrected.

## Eventual coordinated release

Keep public causes standard and index-free. Rust applications must replace old
VfError constructors/accessors, remove arbitrary Clone/Eq assumptions, preserve
per-item OpError certainty/progress, and consume IndexedError explicitly at
scalar boundaries. C consumers continue to use ABI-v3 status layouts; Python
callers receive category/errno/native metadata and explicit item attribution.

The existing manifests/tooling determine coordinated source-breaking versions
when publication is separately authorized. Every promotion must use the same
final main commit for canonical CI and all four port/ABI gates. Then publish the
reviewed cohort, verify downloaded artifacts, repin active ports to actual
published versions, and run their public-package CI before mirror promotion.
