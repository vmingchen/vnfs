#!/usr/bin/env bash
# Required live C SDK contracts; selectors must execute, not silently match zero tests.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
log=$(mktemp)
trap 'rm -f "$log"' EXIT
for test_name in \
  identity::tests::live_nfs_retained_identity_survives_rename_replacement_and_reconnect \
  directory::tests::retained_cohort_children_survive_parent_rename_with_bounded_compounds
do
  cargo test -p vfsi-c --lib "$test_name" -- --ignored --exact 2>&1 | tee "$log"
  if ! grep -Eq '^test result: ok\. 1 passed; 0 failed; 0 ignored;' "$log"; then
    echo "Required C SDK NFS test did not execute: $test_name" >&2
    exit 1
  fi
done
