#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
library=$(realpath "${1:?path to libvfsi_c.so}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cc -Wall -Wextra -Werror -I "$root/bindings/c/include" \
    "$root/bindings/c/tests/dynamic-loader.c" -ldl -o "$work/loader"
"$work/loader"
# This client has no dependency on today's generated public header.
cc -Wall -Wextra -Werror "$root/bindings/c/tests/abi-v3-client.c" \
    "$library" -Wl,-rpath,"$(dirname "$library")" -o "$work/old-client"
mkdir "$work/root"
printf abc >"$work/root/input"
"$work/old-client" "$work/root"
