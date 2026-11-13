#!/usr/bin/env bash
# Required standalone consumer; no direct link to libvfsi_c or test-only ABI.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"
python3 -m unittest discover -s ci -p test_nfs_outcome_fixture.py
include="$repo/bindings/c/include"
consumer_source="$repo/bindings/c/tests/io-outcomes.c"
library=''
legacy_library=${VFSI_OUTCOMES_LEGACY_LIBRARY:-}
server=''
minor=''
output=''
while (($#)); do
  (($# >= 2)) || { echo "Missing value for $1" >&2; exit 2; }
  case "$1" in
    --include) include=$2 ;;
    --consumer-source) consumer_source=$2 ;;
    --library) library=$2 ;;
    --legacy-library) legacy_library=$2 ;;
    --server) server=$2 ;;
    --minor) minor=$2 ;;
    --output) output=$2 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
  shift 2
done
if [[ -n "$server$minor" ]]; then
  [[ -n "$server" && "$minor" =~ ^[12]$ ]] || { echo "Require server and minor 1 or 2" >&2; exit 2; }
fi
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
if [[ -z "$library" ]]; then
  python3 bindings/c/build-sdk.py --profile dev --prefix "$work/current"
  include="$work/current/include"
  library="$work/current/lib/libvfsi_c.so"
fi
# Exercise a real published ABI-v3 library, not a shim hiding new symbols.
# Separate install prefixes prevent the old build overwriting the new library.
if [[ -z "$legacy_library" ]]; then
  curl --proto '=https' --tlsv1.2 --retry 3 -fsSL \
    https://static.crates.io/crates/vfsi-c/vfsi-c-0.3.11.crate -o "$work/old.crate"
  printf '%s  %s\n' 5f4a4af41f88b3f2ade313b38825c9120ae3ed2a8ded6180a058ecd6f556871b "$work/old.crate" | sha256sum -c -
  tar -xzf "$work/old.crate" -C "$work"
  python3 bindings/c/build-sdk.py --registry-only --profile dev \
    --manifest-path "$work/vfsi-c-0.3.11/Cargo.toml" \
    --target-dir "${CARGO_TARGET_DIR:-$repo/target}/outcome-legacy" --prefix "$work/legacy"
  legacy_library="$work/legacy/lib/libvfsi_c.so"
fi
cc -std=c11 -D_XOPEN_SOURCE=700 -Wall -Wextra -Werror -I "$include" \
  "$consumer_source" -ldl -o "$work/consumer"
mkdir "$work/files"
"$work/consumer" "$library" local "$work/files"
"$work/consumer" "$legacy_library" legacy "$work/files"
"$work/consumer" "$legacy_library" posix "$work/files"
"$work/consumer" "$work/no-such-library.so" posix "$work/files"
if [[ -n "$server" ]]; then
  "$work/consumer" "$library" nfs "$server" "$minor"
  report=()
  [[ -z "$output" ]] || report=(--output "$output")
  python3 ci/test-io-outcomes.py --consumer "$work/consumer" --library "$library" \
    --server "$server" --minor "$minor" "${report[@]}"
fi
test -z "$(ls -A "$work/files")"
echo 'Required C outcome conformance passed'
