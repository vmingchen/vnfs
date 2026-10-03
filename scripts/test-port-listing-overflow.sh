#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ports=${VFSI_PORTS_ROOT:?set VFSI_PORTS_ROOT to the port checkout parent}
library=${VFSI_LIBRARY:?set VFSI_LIBRARY to the development adapter shared library}
fixture=$(mktemp -d /tmp/vfsi-listing-overflow.XXXXXX)
trap 'rm -rf "${fixture:?}"' EXIT
cc -shared -fPIC -I "$repo/bindings/c/include" \
    "$repo/bindings/c/tests/listing-overflow.c" -ldl -o "$fixture/legacy.so"
export VFSI_IMPL=dummy VFSI_LIBRARY="$fixture/legacy.so" VFSI_REAL_LIBRARY="$library"
gitbin="$ports/vfsi-port-git/git"
mkdir -p "$fixture/git"
"$gitbin" -C "$fixture/git" init -q
mkdir "$fixture/git/.git/objects/aa" "$fixture/git/.git/objects/bb"
export VFSI_ROOT="$fixture/git" VFSI_MOUNT="$fixture/git"
failed=0
VFSI_IMPL=off "$gitbin" -C "$fixture/git" count-objects -v > "$fixture/expected"
# The aggregate exact-limit case must remain valid across multiple directories.
VFSI_TEST_ENTRIES=200000 "$gitbin" -C "$fixture/git" count-objects -v > "$fixture/exact" 2> "$fixture/exact-error"
grep -q '^garbage: 200000$' "$fixture/exact"
"$gitbin" -C "$fixture/git" count-objects -v > "$fixture/git-output" 2> "$fixture/git-error"
if ! cmp -s "$fixture/expected" "$fixture/git-output"; then
    echo 'FAIL: Git accepted a truncated object scan' >&2
    failed=1
fi
mkdir -p "$fixture/source" "$fixture/destination"
printf 'keep\n' > "$fixture/source/tail"
cp "$fixture/source/tail" "$fixture/destination/tail"
export VFSI_ROOT="$fixture" VFSI_MOUNT="$fixture"
"$ports/vfsi-port-rsync/rsync" -r --delete --dry-run --itemize-changes \
    "$fixture/source/" "$fixture/destination/" > "$fixture/rsync-output"
if grep -q 'deleting.*tail' "$fixture/rsync-output"; then
    echo 'FAIL: rsync would delete a source file omitted by a truncated listing' >&2
    failed=1
fi
# Also verify the real transfer retains the omitted source file and removes
# a genuinely absent destination file on the safe POSIX fallback.
printf 'obsolete\n' > "$fixture/destination/obsolete"
"$ports/vfsi-port-rsync/rsync" -r --delete "$fixture/source/" "$fixture/destination/"
cmp "$fixture/source/tail" "$fixture/destination/tail"
test ! -e "$fixture/destination/obsolete"
if ((failed)); then exit 1; fi
echo 'Listing overflow regressions passed (Git complete fallback and rsync deletion safety).'
