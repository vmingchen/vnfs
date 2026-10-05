# Local CI and build reuse

### Development application ports

After temporarily pointing the Rust ports' `vnfs` dependency at this checkout:

```sh
bash scripts/test-ports-local.sh /home/ubuntu/gh
```

The script verifies dependency resolution, runs shared helper regressions and
Rust port tests, builds Git/rsync against this checkout's C header/library, and
checks C port parity. Configure rsync with `--enable-vfsi` first. It does not
fetch, reset, patch, commit or publish application checkouts. Build artifacts and
Cargo lockfiles may be updated. This checks local/kernel-backed behavior; run
`scripts/test-ports-nfs.sh` separately for NFSv4.1/v4.2 parity with a live export.

The scheduled compatibility workflow still uses pinned published port revisions.
After committing the migrations in their own repositories, update those pins
before claiming scheduled CI coverage for the new port source.

Run these commands from the repository root:

```sh
./scripts/test-ci-local.sh smoke  # Smaller Rust subset for edit/test iteration
./scripts/test-ci-local.sh fast   # Server-independent Rust tests, including faults
./scripts/test-ci-local.sh check  # Formatting and Clippy, including adapters
./scripts/test-ci-local.sh rust   # check + fast
./scripts/test-ci-local.sh quick  # rust + Python tests (existing behavior)
./scripts/test-ci-local.sh full   # rust + Python + live NFS/SMB tests
```

`full` is an alias for `all`, not a replacement for GitHub's package, security,
sanitizer, Kerberos, restart, or patched-server COPY jobs. See `--help` for server
setup. Tests that change server state remain serialized in their existing jobs.

Root and detached Rust workspaces share `target/` by default while retaining
independent lockfiles. Set `CARGO_TARGET_DIR` to select another artifact directory.
Cargo fingerprints source, features, toolchains, and build configuration; sharing
artifacts does not skip tests or substitute one package's sources for another's.
Sanitizer builds continue using separate target directories.

The native dependency test uses a source-content-keyed, independently resolved
fixture under `target/native-tests/`, protected by `flock`. It never edits the
registry source. Changes to the resolved source select a new fixture, and Cargo
handles compiler/build-setting changes. Fixtures persist across runs; removing
the generated `target/` directory restores a cold build.

Each Rust test command records elapsed seconds and exit status in
`target/test-timings.tsv` (override with `VFSI_TEST_TIMINGS`). CI includes that
report in its job summary. Compare both cold and warm runs: the first run after a
cache-key change populates caches and should not be treated as a warm result.

CI caches pinned security executables, not audit results: dependency audits and
advisory updates still execute every run. Native dependencies are installed on
every runner. Cache keys separate Python versions and backend job variants.

### Fast edit/test iterations

Run `./scripts/test-rust.sh --quick` (or `./scripts/test-ci-local.sh smoke`)
for core unit/fault tests and vNFS public API, read, vector and tree regressions.
This deliberately skips live NFS/SMB, backend-wide tests, doctests, FFI and
Python adapters. It reuses Cargo artifacts; the first build can still be slow.
Use `./scripts/test-rust.sh` and the applicable live jobs before pushing.
Changes to protocols, FFI, adapters or features need their targeted suites too.

For a very narrow change, use Cargo's test filter directly, for example:
`cargo test -p vnfs --lib option_layout_tests`.
Timing records remain in `target/test-timings.tsv` (or `VFSI_TEST_TIMINGS`).
