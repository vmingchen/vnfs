#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
state_dir=$(mktemp -d "${TMPDIR:-/tmp}/vnfs-gss.XXXXXX")
sudo install -d -m 755 /export
export_dir=$(sudo mktemp -d /export/vnfs-gss.XXXXXX)
ganesha_bin=${GANESHA_BIN:-ganesha.nfsd}
kdc_pid=
ganesha_pid=

free_tcp_port() {
    python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

nfs_port=$(free_tcp_port)
kdc_port=$(free_tcp_port)
while [[ "$kdc_port" == "$nfs_port" ]]; do
    kdc_port=$(free_tcp_port)
done
gss_host="127.0.0.1:$nfs_port"

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
    sudo rm -rf -- "$export_dir"
    rm -rf -- "$state_dir"
    exit "$status"
}
trap cleanup EXIT

sudo chmod 777 "$export_dir"
install -m 600 "$repo_root/ci/nfs-gss/krb5.conf" "$state_dir/krb5.conf"
install -m 600 "$repo_root/ci/nfs-gss/kdc.conf" "$state_dir/kdc.conf"
install -m 600 "$repo_root/ci/nfs-gss/ganesha.conf" "$state_dir/ganesha.conf"
sed -i \
    -e "s|/tmp/vnfs-gss|$state_dir|g" \
    -e "s|61088|$kdc_port|g" \
    "$state_dir/krb5.conf" "$state_dir/kdc.conf"
sed -i \
    -e "s|/tmp/vnfs-gss|$state_dir|g" \
    -e "s|/export/vnfs-gss-test|$export_dir|g" \
    -e "s|NFS_Port = 2049;|NFS_Port = $nfs_port;|" \
    "$state_dir/ganesha.conf"

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
    "$ganesha_bin" -F -p "$state_dir/ganesha.pid" \
    -L "$state_dir/ganesha.log" -f "$state_dir/ganesha.conf" &
ganesha_pid=$!
for _ in {1..100}; do
    if ! sudo kill -0 "$ganesha_pid" 2>/dev/null; then
        echo "Ganesha exited during RPCSEC_GSS startup" >&2
        exit 1
    fi
    if timeout 1 bash -c ": </dev/tcp/127.0.0.1/$nfs_port" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
timeout 1 bash -c ": </dev/tcp/127.0.0.1/$nfs_port"
if sudo grep -Eq "No export entries found|Failed to load FSAL|Errors processing block" \
    "$state_dir/ganesha.log"; then
    echo "Ganesha did not load the RPCSEC_GSS test export" >&2
    exit 1
fi

cd "$repo_root"
# The trailing dot makes the host name absolute. Without it, some CI runners
# append their cloud search domain before requesting the service ticket.
VNFS_GSS_INTEGRATION=1 \
VNFS_GSS_HOST="$gss_host" \
VNFS_GSS_SERVICE=nfs@localhost. \
KRB5_TRACE="$state_dir/client-krb5.trace" \
MALLOC_PERTURB_=165 \
cargo test -p vnfs --test nfs_gss --features rpcsec-gss -- --test-threads=1

# Verify renewal and reconnect when the original TGT is expired but renewable.
expiry_ccache="FILE:$state_dir/expiry.ccache"
printf 'vnfs-client\n' | \
    KRB5CCNAME="$expiry_ccache" kinit -l 1m -r 2h vnfs-client@VFSI.TEST
VNFS_GSS_HOST="$gss_host" \
VNFS_GSS_SERVICE=nfs@localhost. \
VNFS_GSS_EXPIRY_INTEGRATION=1 \
KRB5CCNAME="$expiry_ccache" \
KRB5_TRACE="$state_dir/expiry-krb5.trace" \
cargo test -p vnfs --test nfs_gss --features rpcsec-gss \
    renewable_ticket_allows_reconnect_after_ticket_expiry -- --exact --test-threads=1

python3 -m venv "$state_dir/python"
"$state_dir/python/bin/pip" install -q 'maturin==1.14.1' \
    'fsspec>=2024.12.0,<2027.0.0'
"$state_dir/python/bin/pip" install -q -e adapters/vfsi-fsspec
(
    cd adapters/nfs4fs
    VIRTUAL_ENV="$state_dir/python" "$state_dir/python/bin/maturin" develop -q
)
VNFS_GSS_HOST="$gss_host" "$state_dir/python/bin/python" - <<'PY'
import os
import fsspec

for authentication in ("krb5", "krb5i"):
    fs = fsspec.filesystem(
        "nfs4",
        host=os.environ["VNFS_GSS_HOST"],
        authentication=authentication,
        service_principal="nfs@localhost.",
        require_secure_authentication=True,
        skip_instance_cache=True,
    )
    path = f"/python-{authentication}"
    fs.pipe_file(path, authentication.encode())
    assert fs.cat_file(path) == authentication.encode()
    fs.close()
PY
