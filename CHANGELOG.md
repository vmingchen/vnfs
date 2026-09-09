# Changelog

This project follows [Semantic Versioning](https://semver.org/) for the
`nfs4fs` package. Rust crate and C ABI versions are tracked independently.

## Unreleased

### Added

- Automatic `fsspec` entry-point discovery for `nfs4` and `vfsi`.
- The protocol-neutral `vfsi` import alias in built wheels.
- URL authority support, including `nfs4://server/path`.
- Python 3.9 through 3.14 compatibility and wheel-install checks in CI.
- Reproducible libntirpc source pinning for Python builds.
- Complete PyPI metadata, license files, operational guidance, and a hardened
  tag-based release workflow.

### Changed

- `nfs4fs` now requires `fsspec>=2024.12.0`.
- Missing-path probes no longer hide authentication or transport failures.
- Invalid roots, compound limits, minor versions, and backends fail fast.

## [0.1.0] - 2026-08-31

- Initial `nfs4fs` release with a vectorized NFSv4.1 backend.

[0.1.0]: https://pypi.org/project/nfs4fs/0.1.0/
