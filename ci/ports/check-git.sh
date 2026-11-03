#!/usr/bin/env bash
set -euo pipefail

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

repository=${1:?repository URL is required}
revision=${2:?revision is required}
root=$(pwd)
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/vfsi-port-git.XXXXXX")
checkout=$work/source
trap 'rm -rf "$work"' EXIT
git init --quiet "$checkout"
git -C "$checkout" remote add origin "$repository"
git -C "$checkout" fetch --quiet --depth=1 origin "$revision"
git -C "$checkout" checkout --quiet --detach FETCH_HEAD

# Git's Rust archive rule links checkout/target/release/libgitcore.a.
# The platform SDK's shared target directory must not redirect that artifact.
env -u CARGO_TARGET_DIR make -C "$checkout" -j2 \
    USE_VFSI=YesPlease \
    VFSI_CFLAGS="-I$root/bindings/c/include" \
    NO_CURL=YesPlease \
    NO_GETTEXT=YesPlease \
    NO_TCLTK=YesPlease \
    git

fixture=$work/fixture
mkdir "$fixture"
git -C "$fixture" init --quiet
git -C "$fixture" config user.email vfsi-ci@example.invalid
git -C "$fixture" config user.name "VFSI CI"
printf 'vfsi port smoke\n' >"$fixture/input"
git -C "$fixture" add input
git -C "$fixture" commit --quiet -m smoke

VFSI_IMPL=dummy \
VFSI_LIBRARY="$root/target/debug/libvfsi_c.so" \
VFSI_ROOT="$fixture" \
VFSI_MOUNT="$fixture" \
"$checkout/git" -C "$fixture" count-objects -v >/dev/null

VFSI_LIBRARY="$root/target/debug/libvfsi_c.so" \
VFSI_INCLUDE="$root/bindings/c/include" \
    sh "$checkout/contrib/vfsi/test-port.sh"
