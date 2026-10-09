# Working in vNFS

Use this repository as the primary development checkout. Keep package boundaries
clear; prefer small, explicit changes over new abstraction or utility frameworks.
Preserve unrelated changes and backup files. Do not push or publish without an
explicit user request.

## Focused testing

Choose tests by changed behavior and its callers, not just changed filenames.
Briefly explain the selected packages, targets, and relevant features before
running them. Use the existing runners where possible.

- During iteration, run new/changed regression tests first, then related tests.
  Select packages and test targets explicitly; a name filter alone does not avoid
  compiling other selected targets.
- Keep deterministic fault-injection and batching/compound-count assertions in
  the relevant selection. Do not disable features merely to reduce test counts.
- Verify that selected tests actually execute. Zero tests, feature-disabled
  cases, ignored tests, or tests that skip missing fixtures are not coverage.
- Reuse the existing build profile, target directory, and feature sets. Do not
  clean build caches or introduce per-command build flags without a reason.
- Widen the selection when callers cross package boundaries, failures suggest
  broader impact, or coverage is uncertain. Do not run every live/backend suite
  after each small edit.

Typical starting points (inspect current tests before selecting):

- Options/errors/request types: `vfsi-core` unit and contract targets, then
  affected `vfsi-sync` and public vNFS tests.
- Native dispatch/ownership: `vfsi-sync` unit and `native_client` targets.
- Local traversal: `vfsi-local` tests and public directory/helper targets.
- Application reads/writes/helpers: the relevant `vnfs` integration target.
- NFS/SMB protocol changes: backend unit/fault tests and relevant live tests with
  required fixtures enabled. Recovery/authentication/ordering changes need live
  coverage where mocks cannot establish correctness.
- Shared traits or public API changes: smoke tests plus compilation/tests for
  affected backends and detached Python adapters.
- CI scripts: `python3 scripts/test-ci-scripts.py` and shell syntax checks.

Before handing off code changes, run `./scripts/test-ci-local.sh smoke` and any
additional checks required by their risk. Prose-only changes need proofreading
and `git diff --check`, not a Rust rebuild. Before a requested push, run the full
server-independent suite (`./scripts/test-rust.sh`) and relevant integration
tests; other release/package checks remain required when applicable. Report
commands, results, timings, and omitted coverage. Do not describe a focused or
smoke run as full CI verification.
