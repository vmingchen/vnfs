#!/usr/bin/env bash
# Validate development ports without fetching, resetting, or editing checkouts.
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ports_root=${1:-${VFSI_PORTS_ROOT:-}}
if [[ -z "$ports_root" ]]; then
    echo 'Usage: bash scripts/test-ports-local.sh DIRECTORY_CONTAINING_VFSI_PORT_REPOS' >&2
    exit 2
fi
ports_root=$(cd -- "$ports_root" && pwd)
for name in coreutils findutils git rsync; do
    [[ -d "$ports_root/vfsi-port-$name/.git" || -f "$ports_root/vfsi-port-$name/.git" ]] || {
        echo "Missing port checkout: vfsi-port-$name" >&2; exit 2;
    }
done
# Refuse to accidentally validate registry builds while claiming dev coverage.
for name in coreutils findutils; do
    cargo metadata --manifest-path "$ports_root/vfsi-port-$name/Cargo.toml" --format-version=1 --features vnfs |
        python3 -c 'import json,pathlib,sys
data=json.load(sys.stdin)
expected=pathlib.Path(sys.argv[1]).resolve()
matches=[p for p in data["packages"] if p["name"]=="vnfs"]
if len(matches)!=1 or pathlib.Path(matches[0]["manifest_path"]).resolve()!=expected:
    raise SystemExit("Port does not resolve vnfs to the development checkout; set its temporary path dependency first")' \
        "$repo_root/crates/vnfs/Cargo.toml"
done
cd "$repo_root"
cargo test -p vnfs --test port_helpers --test directory_pages --test transfer_helpers
cargo test -p vfsi-c --lib
cargo build -p vfsi-c
core="$ports_root/vfsi-port-coreutils"
find="$ports_root/vfsi-port-findutils"
(
    cd "$core"
    cargo test -p uu_cp -p uu_du -p uu_ls -p uucore --lib \
        --features uu_cp/vnfs,uu_du/vnfs,uu_ls/vnfs,uucore/vnfs -- vfsi:: vnfs:: nfs::
    cargo check -p uu_rm -p uu_rmdir --features uu_rm/vnfs,uu_rmdir/vnfs
)
(
    cd "$find"
    cargo test --features vnfs --lib
    cargo check --no-default-features
)
make -C "$ports_root/vfsi-port-git" -j2 USE_VFSI=YesPlease \
    VFSI_CFLAGS="-I$repo_root/bindings/c/include" NO_CURL=YesPlease NO_GETTEXT=YesPlease NO_TCLTK=YesPlease git
# Configure rsync with --enable-vfsi before running this script. Do not silently
# reconfigure the caller's existing checkout or exercise an ordinary-only build.
grep -q '^#define SUPPORT_VFSI 1' "$ports_root/vfsi-port-rsync/config.h" || {
    echo 'rsync must be configured with --enable-vfsi' >&2; exit 2;
}
make -C "$ports_root/vfsi-port-rsync" -j2 CPPFLAGS="-I$repo_root/bindings/c/include"
export VFSI_LIBRARY="${CARGO_TARGET_DIR:-$repo_root/target}/debug/libvfsi_c.so"
bash "$ports_root/vfsi-port-git/contrib/vfsi/test-port.sh"
bash "$ports_root/vfsi-port-rsync/support/test-vfsi-port.sh"
VFSI_PORTS_ROOT="$ports_root" bash "$repo_root/scripts/test-port-listing-overflow.sh"
echo 'All development port compatibility checks passed.'
