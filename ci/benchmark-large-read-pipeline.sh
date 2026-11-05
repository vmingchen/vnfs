#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 HOST EXPORT_ROOT FILE [DELAY] [ROUNDS] [WARMUPS] [CHUNK_SIZES] [WORKER_COUNTS]" >&2
    exit 2
fi

host=$1
root=$2
path=$3
delay=${4:-500us}
rounds=${5:-5}
warmups=${6:-1}
chunk_sizes=${7:-}
worker_counts=${8:-}
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

qdisc_created=0
cleanup() {
    if ((qdisc_created)); then
        sudo tc qdisc del dev "$interface" root 2>/dev/null || true
    fi
}
trap cleanup EXIT
# Bash can run a pending signal trap before the next assignment after tc.
# Record creation ownership before allowing an interrupt to exit.
signal_status=0
add_status=0
trap 'signal_status=130' INT
trap 'signal_status=143' TERM

if sudo tc qdisc add dev "$interface" root netem delay "$delay"; then
    qdisc_created=1
else
    add_status=$?
fi

trap 'exit 130' INT
trap 'exit 143' TERM
((signal_status == 0)) || exit "$signal_status"
((add_status == 0)) || exit "$add_status"
echo "Applying $delay egress delay on $interface (loopback RTT is approximately twice this)."
cd "$repo_root"
benchmark_args=(
    --host "$host" --root "$root" --path "$path" \
    --rounds "$rounds" --warmups "$warmups"
)
if [[ -n "$chunk_sizes" ]]; then
    benchmark_args+=(--chunk-sizes "$chunk_sizes")
fi
if [[ -n "$worker_counts" ]]; then
    benchmark_args+=(--worker-counts "$worker_counts")
fi
cargo run --release -p vnfs --example large_file_read_benchmark --features nfs -- \
    "${benchmark_args[@]}"
