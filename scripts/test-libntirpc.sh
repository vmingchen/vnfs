#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
metadata=$(mktemp /tmp/vfsi-libntirpc-metadata.XXXXXX)
trap 'rm -f "$metadata"' EXIT

# A dependency cannot be selected for unit tests with cargo test -p. Test an
# isolated copy of the exact resolved source, including its dev-dependencies,
# without maintaining another source copy or modifying Cargo's registry cache.
cargo metadata --manifest-path "$repo_root/Cargo.toml" --locked \
    --format-version 1 > "$metadata"
source_dir=$(python3 - "$metadata" <<'PY'
import json
import pathlib
import sys

packages = [p for p in json.loads(pathlib.Path(sys.argv[1]).read_text())["packages"]
            if p["name"] == "libntirpc-sys"]
if len(packages) != 1:
    raise SystemExit("expected exactly one resolved libntirpc-sys package")
print(pathlib.Path(packages[0]["manifest_path"]).parent)
PY
)
source_key=$(tar -C "$source_dir" --sort=name --mtime=@0 --owner=0 --group=0 \
    --exclude=./target --exclude=./.git -cf - . | sha256sum | cut -d' ' -f1)
cache_root="$repo_root/target/native-tests"
mkdir -p "$cache_root"
# Serialize this fixture's builds and source installation, including concurrent
# local CI invocations. Source hashes also invalidate local path overrides.
exec 9>"$cache_root/$source_key.lock"
flock 9
fixture="$cache_root/$source_key"
if [[ ! -f "$fixture/ready" ]]; then
  mkdir -p "$fixture/libntirpc-sys"
  # Local overrides may have a large target directory; copy source only.
  tar -C "$source_dir" --exclude=./target --exclude=./.git -cf - . \
      | tar -C "$fixture/libntirpc-sys" -xf -
  # The persistent fixture lives beneath this repository, but is deliberately
  # independent of its workspace and dependency resolution.
  if ! grep -Eq '^\[workspace\]' "$fixture/libntirpc-sys/Cargo.toml"; then
    printf '\n[workspace]\n' >> "$fixture/libntirpc-sys/Cargo.toml"
  fi
  touch "$fixture/ready"
fi
cargo test --manifest-path "$fixture/libntirpc-sys/Cargo.toml" --all-features "$@"
