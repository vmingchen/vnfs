#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

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
run cargo test -p vnfs --test public_api --test application_boundary --test tree_builder
run cargo test -p vnfs --doc
run cargo test -p vfsi-c --lib

# Python extension crates are intentionally detached Cargo workspaces so their
# maturin distributions retain independent lockfiles. Test them explicitly so
# a successful root-workspace run cannot accidentally omit their Rust code.
run cargo test --manifest-path adapters/vfsi-python/Cargo.toml --locked --all-features
run cargo test --manifest-path adapters/nfs4fs/Cargo.toml --locked
run cargo test --manifest-path adapters/vsmb/Cargo.toml --locked
