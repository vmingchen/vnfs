#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

quick=0
case "${1:-}" in
  "") ;;
  --quick) quick=1; shift ;;
  -h|--help)
    echo 'Usage: scripts/test-rust.sh [--quick]'
    echo '--quick: core unit/fault tests and public API regressions; not a full CI substitute.'
    exit 0 ;;
  *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac
if (($#)); then
  echo 'Unexpected extra arguments' >&2
  exit 2
fi

timings=${VFSI_TEST_TIMINGS:-$repo_root/target/test-timings.tsv}
mkdir -p "$(dirname "$timings")"
printf 'command\tseconds\tstatus\n' > "$timings"
run() {
  local start=$SECONDS status=0
  "$@" || status=$?
  printf '%s\t%s\t%s\n' "$*" "$((SECONDS - start))" "$status" >> "$timings"
  return "$status"
}
# Detached workspaces keep independent lockfiles, but can reuse compatible
# dependency artifacts. Cargo still fingerprints features and build settings.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo_root/target}
run python3 scripts/test-ci-scripts.py

if ((quick)); then
  # Keep deterministic fault coverage and public API contract tests. Skip live
  # servers, backend-wide suites, doctests, FFI and detached Python workspaces.
  run cargo test -p vfsi-core -p vfsi-sync -p vfsi-local --lib \
    --features "vfsi-core/test-faults vfsi-sync/test-faults vfsi-sync/test-support vfsi-local/test-faults"
  run cargo test -p vnfs --features "dummy test-faults" --lib \
    --test public_api --test application_boundary --test tree_builder \
    --test canonical_examples --test readv --test client_vectors --test port_helpers
  exit 0
fi

# Keep this list explicit: several published packages need different feature
# sets, and the live NFS/SMB integration suites run in their dedicated jobs.
# Enable deterministic fault injection in the fast suites too; otherwise
# cfg(feature = "test-faults") regressions are silently omitted.
run cargo test -p vfsi-core -p vfsi-sync -p vfsi-local \
  --features "vfsi-core/test-faults vfsi-sync/test-faults vfsi-sync/test-support vfsi-local/test-faults"
run cargo test -p vfsi-nfs --lib --all-features
run cargo test -p vfsi-smb --lib
run cargo test -p nfsv41-sys
run ./scripts/test-libntirpc.sh
run cargo test -p vnfs --features test-faults --lib
run cargo test -p vnfs --features "dummy test-faults" --test dummy_vecfs
run cargo test -p vnfs --test public_api --test application_boundary --test tree_builder \
  --test transfer_helpers --test canonical_examples --test readv --test client_vectors \
  --test directory_pages --test port_helpers
run cargo test -p vnfs --doc
run cargo test -p vfsi-c --lib

# Python extension crates are intentionally detached Cargo workspaces so their
# maturin distributions retain independent lockfiles. Test them explicitly so
# a successful root-workspace run cannot accidentally omit their Rust code.
run cargo test --manifest-path adapters/vfsi-python/Cargo.toml --locked --all-features
run cargo test --manifest-path adapters/nfs4fs/Cargo.toml --locked
run cargo test --manifest-path adapters/vsmb/Cargo.toml --locked
