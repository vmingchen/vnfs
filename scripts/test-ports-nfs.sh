#!/bin/bash
set -euo pipefail
# Run against an existing loopback Ganesha export. No server configuration is
# changed. The export must permit mounting subdirectories on v4.1 and v4.2.
ports_root=${VFSI_PORTS_ROOT:?set VFSI_PORTS_ROOT to the directory containing the port repos}
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
library=${VFSI_LIBRARY:-$repo_root/target/debug/libvfsi_c.so}
export_root=${VFSI_NFS_EXPORT_DIR:?set VFSI_NFS_EXPORT_DIR to the server export backing directory}
server=${VFSI_NFS_SERVER:-127.0.0.1}
export_path=${VFSI_NFS_EXPORT_PATH:-/}
# Make color parity meaningful even on hosts without a color configuration.
export LS_COLORS="di=01;34:ln=01;36:or=01;31:ex=01;32:*.txt=01;35"
port=${VFSI_NFS_PORT:-2049}
core="$ports_root/vfsi-port-coreutils/target/debug"
findbin="$ports_root/vfsi-port-findutils/target/debug/find"
gitbin="$ports_root/vfsi-port-git/git"
rsyncbin="$ports_root/vfsi-port-rsync/rsync"
for program in "$core/ls" "$core/du" "$core/cp" "$core/rm" "$core/rmdir" "$findbin" "$gitbin" "$rsyncbin"; do
    [[ -x "$program" ]] || { echo "Missing port executable: $program" >&2; exit 2; }
done
[[ -f "$library" ]] || { echo "Missing development C adapter: $library" >&2; exit 2; }
work=$(mktemp -d /tmp/vfsi-port-nfs.XXXXXX)
mountdir="$work/mount"
outputs="$work/output"
fixture=''
mountdir_ready=0
mounted=0
cleanup() {
    local status=$? cleanup_failed=0
    if { ((mounted)) || mountpoint -q "$mountdir"; } && ! sudo umount "$mountdir"; then
        echo "Unmount failed; retained the port test state at $work and $fixture" >&2
        ((status != 0)) || status=1
        exit "$status"
    fi
    # An empty-directory removal cannot recurse into a mount even if its
    # mountpoint check failed. Leave the export fixture intact on failure.
    if ((mountdir_ready)) && ! rmdir -- "$mountdir"; then
        echo "Could not remove the mountpoint; retained $work and $fixture" >&2
        ((status != 0)) || status=1
        exit "$status"
    fi
    if [[ -n "$fixture" ]]; then
        chmod 755 "$fixture/excluded" "$fixture/.hidden" 2>/dev/null || true
        rm -rf -- "$fixture" || cleanup_failed=1
    fi
    rm -rf -- "$outputs" || cleanup_failed=1
    rmdir -- "$work" || cleanup_failed=1
    exit "$((status != 0 ? status : cleanup_failed))"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$mountdir"
mountdir_ready=1
mkdir "$outputs"
fixture=$(mktemp -d "$export_root/vfsi-port-live.XXXXXX")
mkdir -p "$fixture/sub/deep" "$fixture/excluded" "$fixture/.hidden"
printf 'hello\n' > "$fixture/file-1"
printf 'world\n' > "$fixture/sub/file-2"
printf 'deep\n' > "$fixture/sub/deep/file-3"
printf 'excluded\n' > "$fixture/excluded/hidden"
printf 'hidden\n' > "$fixture/.hidden/hidden"
mkdir -p "$fixture/alias-a" "$fixture/alias-b"
printf 'shared\n' > "$fixture/alias-a/shared"
ln "$fixture/alias-a/shared" "$fixture/alias-b/shared"
ln -s file-1 "$fixture/link"
ln -s missing "$fixture/broken-link"

for minor in 1 2; do
    sudo mount -t nfs4 -o "vers=4.$minor,proto=tcp,sec=sys,port=$port,actimeo=0,lookupcache=none" \
        "$server:${export_path%/}/${fixture##*/}" "$mountdir"
    mounted=1
    VNFS_IMPL=off "$core/ls" -R "$mountdir" > "$outputs/kernel-ls"
    VNFS_IMPL=nfs VNFS_STATS=1 "$core/ls" -R "$mountdir" > "$outputs/vfsi-ls" 2> "$outputs/ls-trace"
    grep -Eq "rpc_calls=[1-9][0-9]*" "$outputs/ls-trace"
    diff -u "$outputs/kernel-ls" "$outputs/vfsi-ls"
    # Match native output even when classification needs sparse metadata.
    for flag in -l -li -S -t -lR -s --color=always; do
        VNFS_IMPL=off "$core/ls" "$flag" "$mountdir" > "$outputs/kernel-ls-flags"
        VNFS_IMPL=nfs "$core/ls" "$flag" "$mountdir" > "$outputs/vfsi-ls-flags"
        diff -u "$outputs/kernel-ls-flags" "$outputs/vfsi-ls-flags"
    done
    for order in "alias-a alias-b" "alias-b alias-a"; do
        read -r first second <<< "$order"
        VNFS_IMPL=off "$core/du" -ab "$mountdir/$first" "$mountdir/$second" > "$outputs/kernel-du-multi"
        VNFS_IMPL=nfs "$core/du" -ab "$mountdir/$first" "$mountdir/$second" > "$outputs/vfsi-du-multi"
        diff -u "$outputs/kernel-du-multi" "$outputs/vfsi-du-multi"
    done
    VNFS_IMPL=off "$core/du" -ab --exclude=excluded "$mountdir" | sort > "$outputs/kernel-du"
    VNFS_IMPL=nfs VNFS_STATS=1 "$core/du" -ab --exclude=excluded "$mountdir" | sort > "$outputs/vfsi-du"
    diff -u "$outputs/kernel-du" "$outputs/vfsi-du"
    VNFS_IMPL=off "$findbin" "$mountdir" -name excluded -prune -o -print | sort > "$outputs/kernel-find"
    VNFS_IMPL=nfs VNFS_PROFILE=1 "$findbin" "$mountdir" -name excluded -prune -o -print 2> "$outputs/find-trace" | sort > "$outputs/vfsi-find"
    grep -q "find_walk_completed" "$outputs/find-trace"
    diff -u "$outputs/kernel-find" "$outputs/vfsi-find"
    chmod 000 "$fixture/excluded"
    VNFS_IMPL=nfs "$findbin" "$mountdir" -name excluded -prune -o -print > "$outputs/pruned-find"
    chmod 755 "$fixture/excluded"
    mkdir -p "$outputs/copy-$minor"
    VNFS_IMPL=nfs "$core/cp" "$mountdir/file-1" "$mountdir/sub/file-2" "$outputs/copy-$minor/"
    cmp "$fixture/file-1" "$outputs/copy-$minor/file-1"
    cmp "$fixture/sub/file-2" "$outputs/copy-$minor/file-2"
    mkdir -p "$mountdir/git-$minor"
    "$gitbin" -C "$mountdir/git-$minor" init -q
    "$gitbin" -C "$mountdir/git-$minor" config user.email vfsi-ci@example.invalid
    "$gitbin" -C "$mountdir/git-$minor" config user.name 'VFSI CI'
    printf 'object\n' > "$mountdir/git-$minor/input"
    "$gitbin" -C "$mountdir/git-$minor" add input
    "$gitbin" -C "$mountdir/git-$minor" commit -qm smoke
    VFSI_IMPL=off "$gitbin" -C "$mountdir/git-$minor" count-objects -v > "$outputs/kernel-git"
    VFSI_IMPL=nfs VFSI_TRACE=1 VFSI_PREFETCH_OBJECTS=1 VFSI_LIBRARY="$library" "$gitbin" -C "$mountdir/git-$minor" count-objects -v > "$outputs/vfsi-git" 2> "$outputs/git-trace"
    grep -q "vfsi: listdirs completed" "$outputs/git-trace"
    diff -u "$outputs/kernel-git" "$outputs/vfsi-git"
    mkdir -p "$outputs/rsync-kernel-$minor" "$outputs/rsync-vfsi-$minor"
    VFSI_IMPL=off "$rsyncbin" -a --exclude=excluded/ --exclude="git-*/" \
        "$mountdir/" "$outputs/rsync-kernel-$minor/"
    VFSI_IMPL=nfs VFSI_LIBRARY="$library" VFSI_TRACE=1 "$rsyncbin" \
        -a --exclude=excluded/ --exclude="git-*/" "$mountdir/" "$outputs/rsync-vfsi-$minor/" 2> "$outputs/rsync-trace"
    grep -q "vfsi: listdirs completed" "$outputs/rsync-trace"
    diff -r --no-dereference "$outputs/rsync-kernel-$minor" "$outputs/rsync-vfsi-$minor"
    mkdir -p "$mountdir/remove-a" "$mountdir/remove-b" "$mountdir/empty"
    printf 'keep\n' > "$mountdir/remove-a/keep"
    printf 'keep\n' > "$mountdir/remove-b/keep"
    touch "$mountdir/remove-a/one" "$mountdir/remove-a/two" "$mountdir/remove-b/one"
    VNFS_IMPL=nfs "$core/rm" "$mountdir/remove-a/one" "$mountdir/remove-a/two" "$mountdir/remove-b/one"
    [[ ! -e "$mountdir/remove-a/one" && ! -e "$mountdir/remove-a/two" && ! -e "$mountdir/remove-b/one" ]]
    [[ -f "$mountdir/remove-a/keep" && -f "$mountdir/remove-b/keep" ]]
    VNFS_IMPL=nfs "$core/rmdir" "$mountdir/empty"
    [[ ! -e "$mountdir/empty" ]]
    if VNFS_IMPL=nfs "$core/rmdir" "$mountdir/remove-a/keep" 2> "$outputs/rmdir-file"; then
        echo 'rmdir unexpectedly accepted a regular file' >&2
        exit 1
    fi
    [[ -f "$mountdir/remove-a/keep" ]]
    sudo umount "$mountdir"
    mounted=0
    printf 'Rust and C port parity passed on NFSv4.%s with a subdirectory export.\n' "$minor"
done
