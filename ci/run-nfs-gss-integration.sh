#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
state_dir=/tmp/vnfs-gss
export_dir=/export/vnfs-gss-test
ganesha_bin=${GANESHA_BIN:-ganesha.nfsd}
kdc_pid=
ganesha_pid=

cleanup() {
    status=$?
    if [[ -n "$ganesha_pid" ]]; then
        sudo kill "$ganesha_pid" 2>/dev/null || true
        wait "$ganesha_pid" 2>/dev/null || true
    fi
    if [[ -n "$kdc_pid" ]]; then
        kill "$kdc_pid" 2>/dev/null || true
        wait "$kdc_pid" 2>/dev/null || true
    fi
    if [[ $status -ne 0 ]]; then
        test ! -e "$state_dir/kdc.log" || tail -n 200 "$state_dir/kdc.log"
        test ! -e "$state_dir/ganesha.log" || sudo tail -n 200 "$state_dir/ganesha.log"
        test ! -e "$state_dir/client-krb5.trace" || tail -n 200 "$state_dir/client-krb5.trace"
    fi
    sudo rm -rf "$export_dir"
    exit "$status"
}
trap cleanup EXIT

sudo systemctl stop nfs-ganesha.service 2>/dev/null || true
sudo pkill -TERM -x ganesha.nfsd 2>/dev/null || true
for _ in {1..50}; do
    pgrep -x ganesha.nfsd >/dev/null || break
    sleep 0.1
done
if pgrep -x ganesha.nfsd >/dev/null; then
    echo "existing ganesha.nfsd did not stop" >&2
    exit 1
fi

sudo rm -rf "$state_dir"
install -d -m 755 "$state_dir"
sudo install -d -m 777 "$export_dir"
install -m 600 "$repo_root/ci/nfs-gss/krb5.conf" "$state_dir/krb5.conf"
install -m 600 "$repo_root/ci/nfs-gss/kdc.conf" "$state_dir/kdc.conf"
install -m 600 "$repo_root/ci/nfs-gss/ganesha.conf" "$state_dir/ganesha.conf"

export KRB5_CONFIG="$state_dir/krb5.conf"
export KRB5_KDC_PROFILE="$state_dir/kdc.conf"
export KRB5CCNAME="FILE:$state_dir/client.ccache"

kdb5_util create -s -P vnfs-master -r VFSI.TEST
kadmin.local -r VFSI.TEST -q "addprinc -pw vnfs-client vnfs-client@VFSI.TEST"
kadmin.local -r VFSI.TEST -q "addprinc -randkey nfs/localhost@VFSI.TEST"
kadmin.local -r VFSI.TEST -q "addprinc -randkey nfs/localhost.@VFSI.TEST"
kadmin.local -r VFSI.TEST -q "ktadd -k $state_dir/server.keytab nfs/localhost@VFSI.TEST"
kadmin.local -r VFSI.TEST -q "ktadd -k $state_dir/server.keytab nfs/localhost.@VFSI.TEST"

krb5kdc -n -P "$state_dir/kdc.pid" >"$state_dir/kdc.log" 2>&1 &
kdc_pid=$!
for _ in {1..50}; do
    kill -0 "$kdc_pid" 2>/dev/null || break
    printf 'vnfs-client\n' | kinit vnfs-client@VFSI.TEST 2>/dev/null && break
    sleep 0.1
done
klist -s || {
    echo "failed to obtain the integration-test Kerberos ticket" >&2
    exit 1
}

sudo install -d -m 755 /run/ganesha
sudo env \
    KRB5_CONFIG="$KRB5_CONFIG" \
    KRB5_KTNAME="FILE:$state_dir/server.keytab" \
    LD_LIBRARY_PATH="${GANESHA_LIBRARY_PATH:-}" \
    "$ganesha_bin" -F -L "$state_dir/ganesha.log" -f "$state_dir/ganesha.conf" &
ganesha_pid=$!
for _ in {1..100}; do
    if ! sudo kill -0 "$ganesha_pid" 2>/dev/null; then
        echo "Ganesha exited during RPCSEC_GSS startup" >&2
        exit 1
    fi
    rpcinfo -t 127.0.0.1 nfs 4 >/dev/null 2>&1 && break
    sleep 0.1
done
rpcinfo -t 127.0.0.1 nfs 4 >/dev/null
if sudo grep -Eq "No export entries found|Failed to load FSAL|Errors processing block" \
    "$state_dir/ganesha.log"; then
    echo "Ganesha did not load the RPCSEC_GSS test export" >&2
    exit 1
fi

cd "$repo_root"
VNFS_GSS_INTEGRATION=1 \
VNFS_GSS_HOST=127.0.0.1 \
VNFS_GSS_SERVICE=nfs@localhost \
KRB5_TRACE="$state_dir/client-krb5.trace" \
MALLOC_PERTURB_=165 \
cargo test -p vnfs --test nfs_gss --features rpcsec-gss -- --test-threads=1
