# Changelog

This project follows [Semantic Versioning](https://semver.org/) for the
`nfs4fs` package. Rust crate and C ABI versions are tracked independently.

## Unreleased

### Changed

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

[Unreleased]: https://github.com/vmingchen/vnfs/compare/nfs4fs-v0.3.0...HEAD
[0.3.0]: https://pypi.org/project/nfs4fs/0.3.0/
[0.2.0]: https://pypi.org/project/nfs4fs/0.2.0/
[0.1.0]: https://pypi.org/project/nfs4fs/0.1.0/
