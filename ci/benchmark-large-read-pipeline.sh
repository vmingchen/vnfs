#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 HOST EXPORT_ROOT FILE [DELAY] [ROUNDS] [WARMUPS]" >&2
    exit 2
fi

host=$1
root=$2
path=$3
delay=${4:-500us}
rounds=${5:-5}
warmups=${6:-1}
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
interface=lo

case "$host" in
    127.*|localhost|localhost:*|\[::1\]*|::1)
        ;;
    *)
        echo "latency helper only supports loopback NFS endpoints; refusing to shape shared network interface" >&2
        exit 2
        ;;
esac

if ! tc qdisc show dev "$interface" | grep -Eq '^qdisc noqueue .* root'; then
    echo "refusing to replace an existing root qdisc on $interface" >&2
    tc qdisc show dev "$interface" >&2
    exit 1
fi

cleanup() {
    sudo tc qdisc del dev "$interface" root 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

sudo tc qdisc add dev "$interface" root netem delay "$delay"
echo "Applying $delay egress delay on $interface (loopback RTT is approximately twice this)."
cd "$repo_root"
cargo run --release -p vnfs --example large_file_read_benchmark --features nfs -- \
    --host "$host" --root "$root" --path "$path" \
    --rounds "$rounds" --warmups "$warmups"
