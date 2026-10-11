# Changelog

This project follows [Semantic Versioning](https://semver.org/) for the
`nfs4fs` package. Rust crate and C ABI versions are tracked independently.

## Unreleased

## Python release - 2026-10-10

- nfs4fs 0.3.6 with vfsi-fsspec 0.1.6. Upgrade the protocol package and
  shared engine together; older native extensions lack per-call read budgets.
- Bounded reads include growth after stat, recovery verifies descriptor
  identity, and disabling automatic reconnect also disables native recovery.
- Shared directory removal uses capability-aware metadata, retaining NFS
  no-follow checks while supporting SMB without scalar lstat.

## Rust release - 2026-10-10

- vnfs 0.0.21, vfsi-core 0.1.9, vfsi-sync 0.1.10, vfsi-local 0.1.8,
  vfsi-nfs 0.1.9, and vfsi-smb 0.1.7. First releases of vfsi-posix 0.1.0
  and vfsi-uring 0.1.0; nfsv41-sys and libntirpc-sys are unchanged.
- Separated shared local filesystem machinery from POSIX and batched Linux
  io_uring executors, with direct caller-buffer reads and bounded ring batches.
- Hardened NFS recovery, descriptor cleanup, bounded reads, and Python
  batching; kept regression and fault-injection coverage across backends.

## Coordinated patch release - 2026-10-05

- Rust: vnfs 0.0.19, vfsi-core 0.1.7, vfsi-sync 0.1.8, vfsi-local 0.1.6, vfsi-nfs 0.1.7, vfsi-smb 0.1.5, and vfsi-c 0.3.5.
- Moved high-level scalar and workflow conveniences onto VfsiExt, keeping Vfsi focused on vectorized backend operations.

## Coordinated patch release - 2026-10-04

- Rust: vnfs 0.0.18, vfsi-core 0.1.6, vfsi-sync 0.1.7,
  vfsi-local 0.1.5, vfsi-nfs 0.1.6, vfsi-smb 0.1.4,
  nfsv41-sys 0.1.10, and vfsi-c 0.3.4. libntirpc-sys remains at 0.3.0.
- Python: nfs4fs 0.3.5, vfsi-fsspec 0.1.5, vsmb 0.1.2, and vsmbfs 0.1.2.
- Consolidated native backend contracts and shared workflows, and exposed
  vectorized ownership, attribute, link, and directory creation operations
  through the portable Vfsi API.
- Added bounded copy/move workflows, atomic no-replace rename, reusable mount
  path mapping, ordered/prunable traversal, and streaming to caller-owned
  writers so application ports can use the high-level API.
- Added indexed C listings that distinguish completion from cancellation,
  preserve duplicate operands, reject missing returned metadata, and bound
  first-page fetch cohorts before enforcing aggregate quotas. Callbacks run
  outside backend locks and no partial snapshot is delivered on failure.
- Updated package dependency floors together and added local/live application
  compatibility checks, including fault-injection coverage for listing limits
  and missing metadata.

## Coordinated patch release - 2026-10-02

- Rust: vnfs 0.0.16, vfsi-core 0.1.4, vfsi-sync 0.1.5,
  vfsi-local 0.1.4, vfsi-nfs 0.1.4, vfsi-smb 0.1.3,
  nfsv41-sys 0.1.8, libntirpc-sys 0.2.7, and vfsi-c 0.3.2.
- Python: nfs4fs 0.3.4, vfsi-fsspec 0.1.4, vsmb 0.1.1, and vsmbfs 0.1.1.
- Added bounded pruning-aware traversal with selective metadata, shared Linux
  mount discovery, and bounded C directory and whole-file adapters. Fixed
  late thread-local NFS client cleanup while retaining compound byte checks.
- Application ports now avoid eager subtree scans and unbounded object
  prefetch. Overflow is never treated as a complete Git/rsync listing;
  legacy rsync adapters use POSIX enumeration to preserve deletion safety.
- Dependency floors advance together so published adapters resolve the new
  high-level API rather than historical incompatible implementations.

## Previous unreleased changes

### Changed

- nfs4fs now requires an explicit `auth=` selection at construction. Use
  `auth="auth_sys"` for legacy AUTH_SYS, or `auth="krb5"`/`auth="krb5i"` for
  RPCSEC_GSS. The old `authentication=` spelling and implicit AUTH_SYS fallback
  are no longer accepted. nfs4fs 0.3.3 requires vfsi-fsspec 0.1.3, which
  carries the matching native constructor and process-safe block-cache locks.
- Made nfs4fs reconnects session-scoped and lifecycle-safe: healthy pooled
  sessions remain usable, writable handles are never destructively reopened,
  and failed setup cleanup retains retryable descriptor ownership. Whole-file
  copies keep the vectorized small-file fast path while files beyond the read
  allocation budget use server-side or fallback `copyv` in 16 MiB extents.
  The shared `vfsi-fsspec` engine is versioned independently so protocol
  adapters cannot resolve to an older implementation at install time.
- Hardened nfs4fs for production use: every bytes-returning read now obeys a
  configurable allocation budget, shallow walks stop before traversing deeper
  namespaces, failed CLOSE operations retain retryable ownership, optional
  session pools remove filesystem-wide thread serialization, and the native
  seam rejects malformed backend result cardinality, identity, offsets, and
  lengths without panicking. Python now exposes optional `krb5`/`krb5i`
  RPCSEC_GSS with fail-closed configuration; CI covers Python recovery across
  a live Ganesha restart and both supported Kerberos protection levels.
- Added NFS COMPOUND-response and attribute-list fuzz targets, randomized
  offset-boundary checks for every backend, and CI coverage under Rust ASan
  plus UBSan-instrumented native NFS/XDR shims. Existing fault-injection tests
  cover stalled reads, malformed operation results, cleanup failures,
  post-prefix failures, and local-backend symlink swaps.
- Hardened backend lifecycle and deep-tree behavior: local clients now offer
  fallible construction, SMB clients offer fallible shutdown, and local, NFS,
  and SMB recursive listing/copy/removal use iterative traversal. NFS identity
  lookup is reentrant, bounded, and preserves qualified identity domains; SMB
  concurrent writes verify server file identities to serialize hard-link and
  reparse aliases without disabling vectorization for independent files.
- Added opt-in Kerberos RPCSEC_GSS support to the Rust NFS client behind the
  non-default `rpcsec-gss` feature. Connection options select `krb5` or
  `krb5i`, use the process credential cache without accepting passwords,
  and preserve authentication across automatic reconnects; AUTH_SYS remains
  the compatible default.
- Made native Rust builds use the system `libntirpc` instead of cloning and
  compiling Git sources from `build.rs`; documented Linux, Rust 1.88, native
  package, authentication, and synchronous-API requirements. Wheel builds use
  a checksum-pinned source archive in the explicit release job and retain the
  upstream binary-redistribution notice.
- Added an idiomatic RAII file handle implementing `Read`, `Write`, and `Seek`,
  plus guidance for combining per-worker NFS sessions with vector batches.
- Added bounded automatic recovery for side-effect-free NFS operations after
  transport/session failures. Live path-backed descriptors are reopened in a
  vector with their numeric identities and offsets preserved; mutations are
  never replayed after an ambiguous failure.
- Documented ordered COMPOUND partial-success behavior and the recovery limits
  applications must account for.
- Added reproducible cold- and warm-cache small-file benchmarks for the Rust
  `vnfs` API and Python `nfs4fs` adapter, comparing vectorized operations with
  scalar access through a kernel NFS mount under simulated network latency.
- Made `vnfs` an NFS-focused package by moving Rust SMB ownership and
  integration coverage to `vfsi-smb`; cross-protocol bindings now compose the
  backend crates directly. Package descriptions and registry keywords reflect
  these boundaries.
- Added the `vfsi` discovery keyword to every first-party Rust and Python
  package. The published `vnfs`, `nfs4fs`, and `nfsv41-sys` packages receive
  metadata-release version bumps.
- Split Python SMB support into `vsmb`, a low-level vectorized client with no
  fsspec dependency, and `vsmbfs`, the fsspec adapter. `nfs4fs` is now NFS-only
  and shares its backend-neutral fsspec engine through `vfsi-fsspec`. The Rust
  SMB backend retains the `vfsi-smb` crate name.
- Reorganized the platform by architectural boundary: shared types and sync
  interfaces now live in `vfsi-core` and `vfsi-sync`; NFS, SMB, and local
  implementations live in dedicated backend crates; `vnfs` remains the
  backwards-compatible published facade; and the C and Python packages now
  live under `bindings/` and `adapters/`.
- Added a versioned ecosystem registry, pinned Git and rsync compatibility
  checks, and an approval-gated, fast-forward-only workflow for promoting
  canonical releases to the future `vfsi/vfsi` public mirror.
- Documented the VFSI umbrella identity, `sfsi`/`vfsi`/`afsi`/`tfsi` API
  facets, repository classes, application-port naming, dependency direction,
  and release policy.

## [0.3.0] - 2026-09-12

### Added

- Complete fsspec progress callbacks for vectorized reads, writes, local
  transfers, and server-side copies, including per-file byte progress and
  parent completion tracking.
- Opt-in fsspec directory-listing caching with TTL and LRU limits, forced
  refreshes, cache-aware vectorized traversal, and mutation-safe invalidation.
- Per-open fsspec read buffering with every registered cache type, bounded
  cross-file prefetch for vectorized `OpenFiles` reads, and opt-in buffered
  write/append waves with disk-spooled memory limits.
- A shrinkable Hypothesis state-machine suite that differentially checks
  randomized nfs4fs operation histories against fsspec's local implementation.
- A cache-composition state machine covering listing expiry and invalidation,
  persistent block-cache generations and eviction, long-lived readers,
  `readinto`, and vectorized `OpenFiles` reads against the local oracle.

### Fixed

- nfs4fs now matches `LocalFileSystem` for direct-open path validation,
  existing-directory `mkdir`, timestamp-preserving `touch`, non-recursive
  directory removal errors, negative seeks, and the `read(-1)` range boundary.
- Per-open and persistent fsspec block caches now honor whole-file reads,
  exclusive block boundaries, and clean cache generations after source UID or
  expiry invalidation, preventing sparse zero-fill, mixed generations, stale
  shrink reads, and mmap failures after file growth. Persistent cache entries
  now also remain active for vectorized `OpenFiles` reads instead of being
  bypassed by fsspec's wrapper traversal. Same-client writes, renames, deletes,
  and recursive subtree mutations invalidate future persistent-cache opens
  while preserving already-open handle semantics. Persistent scalar and group
  reopens now reuse generation block sizes, explicit eviction cannot resurrect
  deleted metadata, and open cached handles retain descriptor semantics across
  unlink while ordinary reads remain lazy.
- `rm_file()` now dispatches through nfs4fs's single-file removal path.
- NFS vector lookup batches now distinguish the item that failed from the
  suffix that the server never executed, and safely continue independent
  read-only suffix items in a new compound.
- Compound response validation now rejects malformed or truncated replies,
  adaptively splits batches rejected for server resource limits, and reports
  lost mutating replies as ambiguous without silently replaying them.

## [0.2.0] - 2026-09-09

### Added

- Automatic `fsspec` entry-point discovery for `nfs4` and `vfsi`.
- The protocol-neutral `vfsi` import alias in built wheels.
- URL authority support, including `nfs4://server/path`.
- Python 3.9 through 3.14 compatibility and wheel-install checks in CI.
- Reproducible libntirpc source pinning for Python builds.
- Complete PyPI metadata, license files, operational guidance, and a hardened
  tag-based release workflow.
- Disk-spooled staged fsspec transactions with same-directory temporary files.
- Configurable NFS connection/RPC timeouts, safe read reconnects, process-fork
  recovery, deterministic filesystem shutdown, and context-manager support.
- Public type stubs for the Python and native APIs.
- CPython 3.14 free-threaded build/test coverage, installed-wheel smoke tests,
  dependency audits, and generated CycloneDX Python/Rust SBOMs in CI.

### Changed

- `nfs4fs` now requires `fsspec>=2024.12.0`.
- Missing-path probes no longer hide authentication or transport failures.
- Invalid roots, compound limits, minor versions, and backends fail fast.
- File identity prefers NFS `change` attributes and otherwise uses nanosecond
  timestamps, preventing stale checksums after immediate same-size writes.
- Bulk reads, writes, uploads, and downloads now have explicit item/byte bounds;
  oversized files use bounded streaming chunks.
- Append handles report the real end-of-file position, exclusive creates are
  atomic, negative byte ranges follow fsspec slice semantics, and blocking
  native I/O releases the CPython interpreter lock.

## [0.1.0] - 2026-08-31

- Initial `nfs4fs` release with a vectorized NFSv4.1 backend.

[Unreleased]: https://github.com/vmingchen/vnfs/compare/nfs4fs-v0.3.2...HEAD
[0.3.0]: https://pypi.org/project/nfs4fs/0.3.0/
[0.2.0]: https://pypi.org/project/nfs4fs/0.2.0/
[0.1.0]: https://pypi.org/project/nfs4fs/0.1.0/
