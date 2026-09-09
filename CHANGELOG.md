# Changelog

This project follows [Semantic Versioning](https://semver.org/) for the
`nfs4fs` package. Rust crate and C ABI versions are tracked independently.

## Unreleased

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

[Unreleased]: https://github.com/vmingchen/vnfs/compare/nfs4fs-v0.2.0...HEAD
[0.2.0]: https://pypi.org/project/nfs4fs/0.2.0/
[0.1.0]: https://pypi.org/project/nfs4fs/0.1.0/
