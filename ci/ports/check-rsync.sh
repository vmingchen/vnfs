#!/usr/bin/env bash
set -euo pipefail

repository=${1:?repository URL is required}
revision=${2:?revision is required}
checkout=${RUNNER_TEMP:-/tmp}/vfsi-port-rsync
root=$(pwd)

rm -rf "$checkout"
git init --quiet "$checkout"
git -C "$checkout" remote add origin "$repository"
git -C "$checkout" fetch --quiet --depth=1 origin "$revision"
git -C "$checkout" checkout --quiet --detach FETCH_HEAD

(
    cd "$checkout"
    CPPFLAGS="-I$root/bindings/c/include" ./configure \
        --enable-vfsi \
        --disable-md2man \
        --disable-xxhash \
        --disable-zstd \
        --disable-lz4 \
        --disable-idn
    make -j2
)

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/source" "$fixture/destination"
printf 'vfsi port smoke\n' >"$fixture/source/input"

VFSI_IMPL=dummy \
VFSI_LIBRARY="$root/target/debug/libvfsi_c.so" \
VFSI_ROOT="$fixture" \
VFSI_MOUNT="$fixture" \
"$checkout/rsync" -a "$fixture/source/" "$fixture/destination/"

cmp "$fixture/source/input" "$fixture/destination/input"
