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
port=${VFSI_NFS_PORT:-2049}
core="$ports_root/vfsi-port-coreutils/target/debug"
findbin="$ports_root/vfsi-port-findutils/target/debug/find"
gitbin="$ports_root/vfsi-port-git/git"
rsyncbin="$ports_root/vfsi-port-rsync/rsync"
for program in "$core/ls" "$core/du" "$core/cp" "$findbin" "$gitbin" "$rsyncbin"; do
    [[ -x "$program" ]] || { echo "Missing port executable: $program" >&2; exit 2; }
done
[[ -f "$library" ]] || { echo "Missing development C adapter: $library" >&2; exit 2; }
fixture=$(mktemp -d "$export_root/vfsi-port-live.XXXXXX")
mountdir=$(mktemp -d /tmp/vfsi-port-mount.XXXXXX)
outputs=$(mktemp -d /tmp/vfsi-port-output.XXXXXX)
cleanup() {
    sudo umount "$mountdir" 2>/dev/null || true
    chmod 755 "$fixture/excluded" "$fixture/.hidden" 2>/dev/null || true
    rm -rf "${fixture:?}" "${mountdir:?}" "${outputs:?}"
}
trap cleanup EXIT
mkdir -p "$fixture/sub/deep" "$fixture/excluded" "$fixture/.hidden"
printf 'hello\n' > "$fixture/file-1"
printf 'world\n' > "$fixture/sub/file-2"
printf 'deep\n' > "$fixture/sub/deep/file-3"
printf 'excluded\n' > "$fixture/excluded/hidden"
printf 'hidden\n' > "$fixture/.hidden/hidden"
for minor in 1 2; do
    sudo mount -t nfs4 -o "vers=4.$minor,proto=tcp,sec=sys,port=$port,actimeo=0,lookupcache=none" \
        "$server:${export_path%/}/${fixture##*/}" "$mountdir"
    VNFS_IMPL=off "$core/ls" -R "$mountdir" > "$outputs/kernel-ls"
    VNFS_IMPL=nfs VNFS_STATS=1 "$core/ls" -R "$mountdir" > "$outputs/vfsi-ls"
    diff -u "$outputs/kernel-ls" "$outputs/vfsi-ls"
    VNFS_IMPL=off "$core/du" -ab --exclude=excluded "$mountdir" | sort > "$outputs/kernel-du"
    VNFS_IMPL=nfs VNFS_STATS=1 "$core/du" -ab --exclude=excluded "$mountdir" | sort > "$outputs/vfsi-du"
    diff -u "$outputs/kernel-du" "$outputs/vfsi-du"
    VNFS_IMPL=off "$findbin" "$mountdir" -name excluded -prune -o -print | sort > "$outputs/kernel-find"
    VNFS_IMPL=nfs "$findbin" "$mountdir" -name excluded -prune -o -print | sort > "$outputs/vfsi-find"
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
    VFSI_IMPL=nfs VFSI_LIBRARY="$library" "$gitbin" -C "$mountdir/git-$minor" count-objects -v > "$outputs/vfsi-git"
    diff -u "$outputs/kernel-git" "$outputs/vfsi-git"
    mkdir -p "$outputs/rsync-kernel-$minor" "$outputs/rsync-vfsi-$minor"
    VFSI_IMPL=off "$rsyncbin" -a --exclude=excluded/ --exclude="git-*/" \
        "$mountdir/" "$outputs/rsync-kernel-$minor/"
    VFSI_IMPL=nfs VFSI_LIBRARY="$library" VFSI_VERBOSE=1 "$rsyncbin" \
        -a --exclude=excluded/ --exclude="git-*/" "$mountdir/" "$outputs/rsync-vfsi-$minor/"
    diff -r "$outputs/rsync-kernel-$minor" "$outputs/rsync-vfsi-$minor"
    sudo umount "$mountdir"
    printf 'Rust and C port parity passed on NFSv4.%s with a subdirectory export.\n' "$minor"
done
