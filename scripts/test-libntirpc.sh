#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
fixture=$(mktemp -d /tmp/vfsi-libntirpc-tests.XXXXXX)
trap 'rm -rf "${fixture:?}"' EXIT

# A dependency cannot be selected for unit tests with cargo test -p. Test an
# isolated copy of the exact resolved source, including its dev-dependencies,
# without maintaining another source copy or modifying Cargo's registry cache.
cargo metadata --manifest-path "$repo_root/Cargo.toml" --locked \
    --format-version 1 > "$fixture/metadata.json"
source_dir=$(python3 - "$fixture/metadata.json" <<'PY'
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
mkdir "$fixture/libntirpc-sys"
# Local overrides may have a large target directory; copy source only.
tar -C "$source_dir" --exclude=./target --exclude=./.git -cf - . \
    | tar -C "$fixture/libntirpc-sys" -xf -
cargo test --manifest-path "$fixture/libntirpc-sys/Cargo.toml" --all-features "$@"
