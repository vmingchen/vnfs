#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

quick=0
package='' target='' filter=''
usage() {
  echo 'Usage: scripts/test-rust.sh [--quick | --package NAME (--lib | --test TARGET) [--filter PATTERN]]'
  echo '--quick: core unit/fault tests and public API regressions; not a full CI substitute.'
  echo '--package: one root-workspace package and target, using the normal test features.'
  echo 'Example: scripts/test-rust.sh --package vnfs --test readv --filter budgets'
}
bad_args() { echo "$*" >&2; usage >&2; exit 2; }
while (($#)); do
  case "$1" in
    --quick) ((quick == 0)) || bad_args 'Duplicate --quick'; quick=1; shift ;;
    --lib) [[ -z "$target" ]] || bad_args 'Select exactly one target'; target=--lib; shift ;;
    --package|--test|--filter)
      (($# >= 2)) && [[ -n "$2" && "$2" != -* ]] || bad_args "Missing value for $1"
      case "$1" in
        --package) [[ -z "$package" ]] || bad_args 'Duplicate --package'; package=$2 ;;
        --test) [[ -z "$target" ]] || bad_args 'Select exactly one target'; target=$2 ;;
        --filter) [[ -z "$filter" ]] || bad_args 'Duplicate --filter'; filter=$2 ;;
      esac
      shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) bad_args "Unknown argument: $1" ;;
  esac
done
if [[ -n "$package$target$filter" ]]; then
  ((quick == 0)) && [[ -n "$package" && -n "$target" ]] ||
    bad_args 'Focused mode requires a package and one target, without --quick'
  focused=(cargo test -p "$package")
  case "$package" in
    vfsi-core|vfsi-local|vfsi-posix) focused+=(--features test-faults) ;;
    vfsi-sync) focused+=(--features 'test-faults test-support') ;;
    vnfs) focused+=(--features 'posix test-faults') ;;
    vfsi-nfs) focused+=(--all-features) ;;
    vfsi-smb|vfsi-uring|nfsv41-sys|vfsi-c) ;;
    *) bad_args "Unknown root-workspace package: $package (use cargo directly for detached adapters)" ;;
  esac
  if [[ "$target" == --lib ]]; then focused+=(--lib); else focused+=(--test "$target"); fi
  [[ -z "$filter" ]] || focused+=("$filter")
  # Explicit live selections must not silently succeed without their server.
  case "$package:$target" in
    vfsi-nfs:nfs|vnfs:tree_builder_nfs|vnfs:transfer_helpers_nfs)
      [[ -n "${VFSI_NFS_SERVER:-}" ]] || bad_args 'Set VFSI_NFS_SERVER for live NFS tests'
      export VFSI_NFS_REQUIRED=1 ;;
    vfsi-smb:smb)
      [[ -n "${VFSI_SMB_SERVER:-}" && -n "${VFSI_SMB_SHARE:-}" ]] ||
        bad_args 'Set VFSI_SMB_SERVER and VFSI_SMB_SHARE for live SMB tests'
      export VFSI_SMB_REQUIRED=1
      focused+=(--features test-faults) ;;
  esac
  focused+=(-- --nocapture)
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

if [[ -n "$package" ]]; then
  focus_log=$(mktemp)
  trap 'rm -f -- "$focus_log"' EXIT
  run_focused() {
    local status=0 ran=0 skipped=0 line
    "$@" 2>&1 | tee "$focus_log" || status=$?
    ((status == 0)) || return "$status"
    while IFS= read -r line; do
      [[ "$line" =~ ^test\ result:\ ok\.\ [1-9][0-9]*\ passed\; ]] && ran=1
      [[ ! "$line" =~ (^|[[:space:]])skipping[[:space:]].*test: ]] || skipped=1
    done < "$focus_log"
    if ((ran == 0 || skipped)); then
      echo 'Focused selection did not execute tests or skipped coverage; check the filter, features and fixtures.' >&2
      return 1
    fi
  }
  printf 'Focused command (not full CI):'; printf ' %q' "${focused[@]}"; printf '\n'
  run run_focused "${focused[@]}"
  exit 0
fi

if ((quick)); then
  # Keep deterministic fault coverage and public API contract tests. Skip live
  # servers, backend-wide suites, doctests, FFI and detached Python workspaces.
  run cargo test -p vfsi-core -p vfsi-sync -p vfsi-local -p vfsi-posix --lib \
    --features "vfsi-core/test-faults vfsi-sync/test-faults vfsi-sync/test-support vfsi-local/test-faults"
  run cargo test -p vnfs --features "posix test-faults" --lib \
    --test public_api --test application_boundary --test tree_builder \
    --test canonical_examples --test readv --test client_vectors --test port_helpers
  exit 0
fi

# Keep this list explicit: several published packages need different feature
# sets, and the live NFS/SMB integration suites run in their dedicated jobs.
# Enable deterministic fault injection in the fast suites too; otherwise
# cfg(feature = "test-faults") regressions are silently omitted.
run cargo test -p vfsi-core -p vfsi-sync -p vfsi-local -p vfsi-posix \
  --features "vfsi-core/test-faults vfsi-sync/test-faults vfsi-sync/test-support vfsi-local/test-faults"
run cargo test -p vfsi-nfs --lib --all-features
run cargo test -p vfsi-smb --lib
run cargo test -p nfsv41-sys
run ./scripts/test-libntirpc.sh
run cargo test -p vnfs --features test-faults --lib
run cargo test -p vnfs --features "posix test-faults" --test posix_backend
run cargo test -p vnfs --test public_api --test application_boundary --test tree_builder \
  --test transfer_helpers --test canonical_examples --test readv --test client_vectors \
  --test directory_pages --test port_helpers --test posix
run cargo test -p vnfs --doc
run cargo test -p vfsi-c --lib

# Python extension crates are intentionally detached Cargo workspaces so their
# maturin distributions retain independent lockfiles. Test them explicitly so
# a successful root-workspace run cannot accidentally omit their Rust code.
run cargo test --manifest-path adapters/vfsi-python/Cargo.toml --locked --all-features
run cargo test --manifest-path adapters/nfs4fs/Cargo.toml --locked
run cargo test --manifest-path adapters/vsmb/Cargo.toml --locked
