# Local CI and build reuse

## Paired benchmarks

`benchmark-paired.py` compares immutable commands in alternating AB/BA pairs on
one fixture, recording executable/input hashes, device/inode identities, raw
output and paired speedup estimates. Build both executables using the same
harness/compiler settings and retain build provenance. Initialize outside timing:

```sh
python3 scripts/benchmark-paired.py --pairs 9 --cache-state advisory-evicted \
  --fixture /tmp/uring-shared --output /tmp/uring-comparison \
  --prepare '/path/baseline 1 target uring cold-small-read --fixture {fixture} --json {report}' \
  --baseline '/path/baseline 100 target uring cold-small-read --cold-rounds 100 --fixture {fixture} --json {report}' \
  --candidate '/path/candidate 100 target uring cold-small-read --cold-rounds 100 --fixture {fixture} --json {report}'
```

Structured comparisons use inner-operation p50 and require the same cases,
reported request geometry, and fixture identities in both commands across all
pairs. Process startup and setup are reported separately. Queue/window
settings may differ for tuning. Read-only preparation should report zero writes
after initialization. A fixture lock excludes overlapping comparisons. Process
groups have deadlines; failures retain logs.

Application ports use the same runner. For read-only deterministic commands,
`--check-output` verifies identical stdout, for example on an existing NFS tree:

```sh
python3 scripts/benchmark-paired.py --cache-state warm --check-output \
  --fixture /mnt/nfs/benchmark-tree --output /tmp/ls-comparison \
  --baseline '/path/upstream/ls -R {fixture}' \
  --candidate 'env VNFS_IMPL=nfs /path/vfsi-port-coreutils/ls -R {fixture}'
```

Explicit `--before-each` setup runs outside timing and gets a separate
`{report}` path for its own output. The runner never changes
shared mounts/network configuration. `--rtt-ms` only labels externally established
RTT: use a dedicated namespace for latency experiments. Cache labels likewise
describe caller setup, not guaranteed cold storage. Keep credentials out of
recorded arguments. Optional `--max-regression-percent 10` gates only when the
estimated speedup interval lies wholly beyond that slowdown; use controlled
hosts. Bootstrap intervals are not proof of equivalence. Keep deterministic
batching tests in CI and timing campaigns separate.

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
`bash scripts/test-ports-nfs.sh` separately for NFSv4.1/v4.2 parity with a live export.

The compatibility workflow runs on relevant pushes on any branch and pull
requests, schedule and manual dispatch. It derives all four application jobs from
`ecosystem/ports.toml`. It tests their exact committed revisions in disposable
checkouts against the platform source, not just their published dependencies.
Uncommitted port edits are excluded. Git/rsync use the development C SDK;
coreutils/findutils run library tests and feature-enabled builds, and findutils
has CLI parity checks. This is local/POSIX coverage; live NFS remains separate.
`ecosystem/versions.toml` records the published package and registered port baseline.
A separate workflow job retains frozen ABI-v3 old-client and optional-symbol checks.
Mirror promotion requires successful CI and compatibility runs for the same main
commit, including completed, successful registry, ABI-v3, and Git, rsync,
coreutils, and findutils jobs. Missing or skipped jobs fail the gate. Check
readiness with `python3 ci/check-promotion.py COMMIT`; for a path-filtered docs-only
commit, manually dispatch the port workflow first.

Run these commands from the repository root:

```sh
./scripts/test-ci-local.sh        # Default: quick Rust unit/fault tests only
./scripts/test-ci-local.sh fast   # Same as the default; smoke/quick are aliases
./scripts/test-ci-local.sh check  # Formatting and Clippy, including adapters
./scripts/test-ci-local.sh rust   # Explicit check + full server-independent Rust
./scripts/test-ci-local.sh full   # Explicit rust + Python + live NFS/SMB tests
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

Each Rust test command records elapsed seconds (millisecond precision on Bash 5+)
and exit status in
`target/test-timings.tsv` (override with `VFSI_TEST_TIMINGS`). CI includes that
report in its job summary. Compare both cold and warm runs: the first run after a
cache-key change populates caches and should not be treated as a warm result.

CI caches pinned security executables, not audit results: dependency audits and
advisory updates still execute every run. Native dependencies are installed on
every runner. Cache keys separate Python versions and backend job variants.

### Fast edit/test iterations

Run `./scripts/test-rust.sh` or `./scripts/test-rust.sh --fast` (equivalently,
`./scripts/test-ci-local.sh fast`) for core, sync, local, and POSIX library
unit tests with fault injection enabled. Two profiled large-tree stress cases
are excluded only from fast mode; full mode and focused selections retain them.
No facade integration targets, native RPC/SMB dependencies, doctests, FFI, or
Python adapters are built. Reuse existing
Cargo artifacts; the first build after source/toolchain changes can still be slow.
`--quick` and local `smoke`/`quick` now mean this same unit-only subset.

Use fast plus focused regression tests during development. Full suites run in
CI or on explicit request, rather than automatically before every commit/push.
CI invokes `./scripts/test-rust.sh --full`, retaining all server-independent
backend, public-API, native dependency, C SDK, doctest, and detached adapter tests.
Explicit local `rust` adds formatting/Clippy; local `full` also runs Python and
live NFS/SMB. Protocol, FFI, adapter, and feature changes still need their focused
coverage. Fast mode is not a full-CI substitute.

For a narrow change, select the package, target and optional name filter through
the focused runner, which retains the package's normal fault-test features:

```sh
./scripts/test-rust.sh --package vnfs --test directory_pages
./scripts/test-rust.sh --package vnfs --lib --filter read::tests
```

The runner records timings in `target/test-timings.tsv` (or `VFSI_TEST_TIMINGS`)
and rejects selections that execute no tests or report missing-fixture skips.
Direct `cargo test` commands do not update this timing report.

### Test runner profile: November 15, 2026

Measured serially on the four-CPU ARM64 VM with Rust 1.93.1, reusing the existing
target directory and default build profile. These are warm-cache wall times,
including Cargo startup. The fast subset deliberately provides less coverage.

| Runner | Median | Samples |
|---|---:|---:|
| Previous smoke (unit + public integration targets) | 4.40s | 3 |
| Previous full server-independent Rust | 20.10s | 3 |
| New fast (175 quick unit tests) | 0.15s | 5 |
| New full server-independent Rust | 19.84s | 3 |

Most first-observed elapsed time was compilation/linking, not test execution:
the previous smoke took 44.74s and full took 138.79s before warming the relevant
artifacts. No caches were cleared, so these are not cold-build comparisons.
An isolated per-test profile showed that the large-tree copy and deep-recursion
stress cases consumed over 90% of the sequential, isolated local-unit profile. Fast mode filters only
those two cases; full mode retains them without filters. Full-mode facade targets
share their feature configuration to avoid redundant rebuilds; the warm full
medians remain within run-to-run variation, so no full-suite speedup is claimed.

To reproduce, select the mode explicitly and reuse the same target directory:

```sh
profile=$(mktemp -d /tmp/vnfs-test-profile.XXXXXX)
VFSI_TEST_TIMINGS="$profile/fast.tsv" /usr/bin/time -v ./scripts/test-rust.sh --fast
# Full profiling is opt-in; CI already selects --full.
VFSI_TEST_TIMINGS="$profile/full.tsv" /usr/bin/time -v ./scripts/test-rust.sh --full
```

Repeat warm runs serially and compare medians. Raw wall-time, memory, command
timings, and the per-test profile are recorded in the
[profile data](../docs/benchmarks/test-runner-2026-11-15.json).
