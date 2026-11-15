# Working in vNFS

Use this repository as the primary development checkout. Keep package boundaries
clear; prefer small, explicit changes over new abstraction or utility frameworks.
Preserve unrelated changes and backup files. Do not push or publish without an
explicit user request.

## Temporary directories on the VM

Create temporary work directories under `/tmp` on the VM, rather than
under `~`. Use a unique task directory, for example
`mktemp -d /tmp/vnfs-task.XXXXXX`. Reuse existing build caches instead of
creating a fresh target directory for each task. Keep durable release, CI, and
benchmark records under `~/Documents/vfsi-artifacts/`, copying them there
before cleaning up temporary files. Remove only task-owned scratch directories
when they are no longer needed; preserve uncommitted source changes.

## Focused testing

Choose tests by changed behavior and its callers, not just changed filenames.
Briefly explain the selected packages, targets, and features. Start with changed
regressions, then related coverage; widen the selection when boundaries cross
packages, failures suggest broader impact, or coverage is uncertain.

- Use `scripts/test-rust.sh --package NAME (--lib | --test TARGET)` for a
  focused root-workspace selection; add `--filter PATTERN` when useful. The
  runner supplies its normal test features and rejects zero-test or explicitly
  skipped selections. A Cargo name filter alone does not avoid compiling other
  selected targets. Detached adapters need their own manifest commands.
- Shared trait or public API changes also need compilation/tests for affected
  backends and language bindings, beyond the starting target.
- Keep deterministic fault-injection and batching/compound-count assertions.
  Zero tests, disabled features, ignored cases, and missing-fixture skips are
  not coverage. Required live fixtures must be enabled explicitly.
- Reuse the existing build profile, target directory, and feature sets. Do not
  clean build caches or introduce per-command build flags without a reason.
- Run relevant backend integration tests for protocol, recovery, authentication,
  ordering, and ownership changes that mocks cannot establish. Do not repeat
  every live suite after each small edit.

Choose the starting layer from the current boundaries:

- Portable `Vfsi`/`VfsiExt` contracts, composed helpers, requests, options, and
  errors: `vfsi-core` unit/contract tests, including `portable_api`; then affected
  `vfsi-sync` and public `vnfs` targets. Portable helpers live in
  `crates/vfsi-core/src/api`.
- Native dispatch and handle ownership: `vfsi-sync` unit tests and `native_client`.
- Local namespace and traversal: `vfsi-local`, followed by relevant
  `vfsi-posix` and public directory/helper targets. For io_uring execution changes,
  also run the real-ring `uring` job on a Linux host that permits io_uring.
- NFS facade, Auto routing, and application regressions: relevant `vnfs` targets.
  NFS/SMB protocol changes also need backend unit/fault and live integration tests.
- Python behavior: affected adapter/shared-engine pytest targets, rebuilding the
  native extension when needed. The Rust adapter workspaces at
  `adapters/vfsi-python`, `adapters/nfs4fs`, and `adapters/vsmb` are detached;
  root `cargo test --workspace` omits them. The full Rust runner tests them
  explicitly.
- C SDK changes: `vfsi-c` tests plus relevant header, ABI-v3, and optional-symbol
  checks under `ci/ports`.
- CI scripts: `python3 scripts/test-ci-scripts.py` and affected shell syntax checks.

Examples:

```sh
./scripts/test-rust.sh --package vfsi-core --test portable_api
./scripts/test-rust.sh --package vfsi-sync --test native_client
./scripts/test-rust.sh --package vfsi-local --lib
```

## Validation and runner scope

Use `./scripts/test-rust.sh --fast` (also the no-argument default) for normal
local iteration and handoff, plus focused tests for changed behavior. Fast mode
runs core, sync, local, and POSIX library unit/fault tests, excluding two profiled
large-tree stress cases retained in full/focused runs. It does not build
NFS/SMB, the C SDK, or Python extensions, or run integration tests and doctests.

Run full suites in CI or when the user explicitly requests them. Do not run full
suites automatically before handing off, committing, or pushing. For changes
outside fast coverage, select relevant package/target, adapter, or live tests;
do not treat a fast pass as validation of those layers. Prose-only changes need
proofreading and `git diff --check`, not a Rust rebuild.

- `scripts/test-rust.sh --full`: complete server-independent Rust coverage,
  including backend unit/fault tests, public contracts, doctests, the C SDK,
  native dependency tests, and detached adapters. CI selects this explicitly.
- `scripts/test-ci-local.sh` defaults to `fast`; `smoke` and `quick` are aliases.
- Local `check` is formatting/Clippy; explicit `rust` is `check` plus full Rust.
- Local `full`/`all` explicitly runs Rust, Python, NFS, and SMB. Separate
  io_uring, packaging, security, sanitizers, Kerberos, restart, and patched-server
  COPY jobs remain in the workflows. Preserve those CI gates.
- For runner changes, run `python3 scripts/test-ci-scripts.py`; fast and focused
  Rust runs avoid paying for unrelated script-plumbing tests on each invocation.

The full local NFS job requires local rpcbind registration for NFS program
100003, version 4, TCP/2049; a direct-port-only server is insufficient for that
runner. Follow `scripts/test-ci-local.sh --help` for fixture requirements.
Managed servers are opt-in; preserve existing services.

Report commands, results, timings, and omitted coverage. Never describe focused,
fast, or local `all` results as full hosted CI verification. Timings are written
to `target/test-timings.tsv`; override `VFSI_TEST_TIMINGS` for separate profiles.
See `scripts/README.md` for runner details.
