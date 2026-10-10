#!/usr/bin/env bash
# Run selected CI checks without changing host service configuration.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

usage() {
  cat <<'HELP'
Usage: scripts/test-ci-local.sh [--managed-nfs] [--managed-smb] JOB [JOB ...]

Jobs: smoke, fast, check, rust, python, nfs, smb, uring, quick (rust + python), all/full
uring: Linux real-ring tests (requires io_uring permitted by kernel/seccomp).
smoke: small server-independent Rust subset for iteration (not full CI).
fast: server-independent Rust tests; check: formatting and Clippy.
rust: check + fast. all/full: rust + python + nfs + smb.

The managed server flags are opt-in. Without them, nfs needs a server at
VFSI_NFS_SERVER (default 127.0.0.1), and smb needs VFSI_SMB_SERVER and
VFSI_SMB_SHARE. Managed NFS refuses to take over an occupied TCP/2049;
managed Samba uses an unprivileged high port (default 14445). Both use
temporary configuration and stop only their own processes on exit.
The full NFS Rust suite also needs the local server registered with rpcbind
as NFS program 100003 version 4, TCP/2049. A direct-port daemon alone is
insufficient because some tests connect through rpcbind.

Set VFSI_LOCAL_CI_VENV to reuse another virtualenv. Otherwise the repo's
.venv is reused, or a temporary virtualenv is created and bootstrapped.
These jobs reproduce the main tests, not packaging, security audits,
sanitizers, Kerberos, server restarts, or the patched-Ganesha COPY job.
See .github/workflows/ci.yml for those independent CI jobs.
HELP
}

managed_nfs=0
managed_smb=0
jobs=()
while (($#)); do
  case "$1" in
    --managed-nfs) managed_nfs=1 ;;
    --managed-smb) managed_smb=1 ;;
    -h|--help) usage; exit 0 ;;
    smoke|fast|check|rust|python|nfs|smb|uring) jobs+=("$1") ;;
    quick) jobs+=(rust python) ;;
    all|full) jobs+=(rust python nfs smb) ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done
if ((${#jobs[@]} == 0)); then
  usage >&2
  exit 2
fi

declare -A selected=()
for job in "${jobs[@]}"; do selected["$job"]=1; done
if [[ -v selected[rust] ]]; then
  selected[check]=1
  selected[fast]=1
fi
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo_root/target}
if ((managed_nfs)) && [[ ! -v selected[nfs] ]]; then
  echo '--managed-nfs requires the nfs job' >&2
  exit 2
fi
if ((managed_smb)) && [[ ! -v selected[smb] ]]; then
  echo '--managed-smb requires the smb job' >&2
  exit 2
fi

state_dir=$(mktemp -d /tmp/vnfs-local-ci.XXXXXXXX)
nfs_launcher_pid=''
nfs_server_pid=''
smb_server_pid=''

stop_owned() {
  local pid=$1 marker=$2 elevated=$3 args
  [[ -n "$pid" ]] || return 0
  args=$(ps -p "$pid" -o args= 2>/dev/null || true)
  [[ "$args" == *"$marker"* ]] || return 0
  if ((elevated)); then
    sudo -n kill -TERM "$pid" 2>/dev/null || true
  else
    kill -TERM "$pid" 2>/dev/null || true
  fi
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  stop_owned "$smb_server_pid" "$state_dir/smb.conf" 0
  stop_owned "$nfs_server_pid" "$state_dir/nfs.conf" 1
  stop_owned "$nfs_launcher_pid" "$state_dir/nfs.conf" 0
  [[ -z "$smb_server_pid" ]] || wait "$smb_server_pid" 2>/dev/null || true
  [[ -z "$nfs_launcher_pid" ]] || wait "$nfs_launcher_pid" 2>/dev/null || true
  # The server exports only this mktemp directory. Leave it on failure for
  # diagnostics; never recursively remove an arbitrary caller-supplied path.
  if ((status == 0)); then
    rm -r -- "$state_dir" 2>/dev/null ||
      echo "Could not remove temporary state: $state_dir" >&2
  else
    echo "Local CI state and server logs: $state_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

port_open() {
  python3 - "$1" <<'PY'
import socket
import sys

with socket.socket() as sock:
    sock.settimeout(0.25)
    raise SystemExit(0 if sock.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)
PY
}

wait_for_port() {
  local port=$1 pid=$2 log=$3
  for _ in {1..60}; do
    if port_open "$port"; then return 0; fi
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    sleep 0.5
  done
  echo "Server did not become ready on TCP/$port" >&2
  [[ ! -f "$log" ]] || tail -n 80 "$log" >&2
  return 1
}

start_nfs() {
  if port_open 2049; then
    echo 'TCP/2049 is occupied; refusing to replace an existing NFS server' >&2
    return 1
  fi
  command -v ganesha.nfsd >/dev/null || { echo 'Install nfs-ganesha and nfs-ganesha-vfs' >&2; return 1; }
  sudo -n true || { echo 'Managed NFS requires noninteractive sudo' >&2; return 1; }
  mkdir -p "$state_dir/nfs-export/git"
  chmod 0777 "$state_dir/nfs-export" "$state_dir/nfs-export/git"
  cat >"$state_dir/nfs.conf" <<EOF
NFS_Core_Param {
  Protocols = 4;
  Enable_NLM = false;
  Enable_RQUOTA = false;
}
NFSV4 { Graceless = true; Minor_Versions = 1, 2; Only_Numeric_Owners = true; }
EXPORT {
  Export_Id = 7707;
  Path = $state_dir/nfs-export;
  Pseudo = /;
  Access_Type = RW;
  Squash = None;
  SecType = sys;
  Protocols = 4;
  Transports = TCP;
  FSAL { Name = VFS; }
}
EOF
  sudo -n ganesha.nfsd -F -p "$state_dir/nfs.pid" -L "$state_dir/nfs.log" \
    -f "$state_dir/nfs.conf" >"$state_dir/nfs.stdout" 2>&1 &
  nfs_launcher_pid=$!
  wait_for_port 2049 "$nfs_launcher_pid" "$state_dir/nfs.log"
  if [[ -f "$state_dir/nfs.pid" ]]; then
    read -r nfs_server_pid <"$state_dir/nfs.pid"
  fi
  export VFSI_NFS_SERVER=127.0.0.1 VFSI_NFS_REQUIRED=1
}

start_smb() {
  local port=${VFSI_LOCAL_CI_SMB_PORT:-14445}
  [[ "$port" =~ ^[0-9]+$ ]] && ((port >= 1024 && port <= 65535)) || {
    echo 'VFSI_LOCAL_CI_SMB_PORT must be a high TCP port' >&2; return 2;
  }
  if port_open "$port"; then
    echo "TCP/$port is occupied; refusing to replace an existing SMB server" >&2
    return 1
  fi
  command -v smbd >/dev/null || { echo 'Install Samba (smbd)' >&2; return 1; }
  mkdir -p "$state_dir/smb-share" "$state_dir/smb-run" "$state_dir/smb-lock" \
    "$state_dir/smb-state" "$state_dir/smb-cache" "$state_dir/smb-private" \
    "$state_dir/smb-ncalrpc"
  chmod 0777 "$state_dir/smb-share"
  cat >"$state_dir/smb.conf" <<EOF
[global]
  server role = standalone server
  workgroup = WORKGROUP
  security = user
  map to guest = Bad User
  guest account = $(id -un)
  server min protocol = SMB2_10
  server max protocol = SMB3_11
  interfaces = lo
  bind interfaces only = yes
  disable netbios = yes
  smb ports = $port
  log file = $state_dir/smbd.log
  enable core files = no
  pid directory = $state_dir/smb-run
  lock directory = $state_dir/smb-lock
  state directory = $state_dir/smb-state
  cache directory = $state_dir/smb-cache
  private dir = $state_dir/smb-private
  ncalrpc dir = $state_dir/smb-ncalrpc

[vfsi-test]
  path = $state_dir/smb-share
  read only = no
  guest ok = yes
  force user = $(id -un)
  create mask = 0666
  directory mask = 0777
EOF
  smbd --foreground --configfile="$state_dir/smb.conf" \
    --port="$port" >"$state_dir/smb.log" 2>&1 &
  smb_server_pid=$!
  wait_for_port "$port" "$smb_server_pid" "$state_dir/smb.log"
  export VFSI_SMB_SERVER="127.0.0.1:$port" VFSI_SMB_SHARE=vfsi-test
  export VFSI_SMB_USERNAME='' VFSI_SMB_PASSWORD='' VFSI_SMB_REQUIRED=1
  export VFSI_SMB_EXPECT_SERVER_COPY=0 VFSI_SMB_REQUIRE_RECONNECT=0
  export VFSI_SMB_LOCAL_ROOT="$state_dir/smb-share"
}

prepare_python() {
  [[ -z "${python_bin:-}" ]] || return 0
  local venv=${VFSI_LOCAL_CI_VENV:-$repo_root/.venv}
  if [[ ! -x "$venv/bin/python" ]]; then
    venv="$state_dir/venv"
    python3 -m venv "$venv"
    "$venv/bin/python" -m pip install maturin fsspec hypothesis pytest
  fi
  python_bin="$venv/bin/python"
  maturin_bin="$venv/bin/maturin"
  [[ -x "$maturin_bin" ]] || { echo "Install maturin in $venv" >&2; return 1; }
  "$python_bin" -m pip install --no-deps -e adapters/vfsi-fsspec -q
  "$maturin_bin" develop --manifest-path adapters/nfs4fs/Cargo.toml --locked
  "$python_bin" -m pip install --no-deps -e adapters/vfsi-fsspec -q
}

prepare_smb_python() {
  prepare_python
  "$maturin_bin" develop --manifest-path adapters/vsmb/Cargo.toml --locked
  "$python_bin" -m pip install --no-deps -e adapters/vsmbfs -q
  "$python_bin" -m pip install --no-deps -e adapters/vfsi-fsspec -q
}

run_check() {
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo fmt --manifest-path adapters/vfsi-python/Cargo.toml --check
  cargo clippy --manifest-path adapters/vfsi-python/Cargo.toml --all-features --all-targets -- -D warnings
  cargo fmt --manifest-path adapters/nfs4fs/Cargo.toml --check
  cargo clippy --manifest-path adapters/nfs4fs/Cargo.toml --locked --all-targets -- -D warnings
  cargo fmt --manifest-path adapters/vsmb/Cargo.toml --check
  cargo clippy --manifest-path adapters/vsmb/Cargo.toml --locked --all-targets -- -D warnings
}

run_fast() { ./scripts/test-rust.sh; }
run_smoke() { ./scripts/test-rust.sh --quick; }

run_uring() {
  [[ $(uname -s) == Linux ]] || { echo 'uring tests require Linux' >&2; return 1; }
  cargo test -p vfsi-uring --locked
  cargo test -p vnfs --locked --no-default-features --features uring --test uring
  cargo test -p vnfs --locked --no-default-features --features uring,posix --example uring_bench
  cargo test -p vnfs --locked --no-default-features --features uring --doc guides::uring
}

run_python() {
  prepare_python
  "$python_bin" -m pytest adapters/vfsi-fsspec/tests
  "$python_bin" -m pytest adapters/nfs4fs/tests
  prepare_smb_python
  "$python_bin" -m pytest adapters/vsmb/tests/test_client.py
  "$python_bin" -m pytest adapters/vsmbfs/tests/test_package.py
}

run_nfs() {
  ((managed_nfs == 0)) || start_nfs
  export VFSI_NFS_SERVER=${VFSI_NFS_SERVER:-127.0.0.1} VFSI_NFS_REQUIRED=1
  if ! command -v rpcinfo >/dev/null ||
     ! rpcinfo -p 127.0.0.1 2>/dev/null |
       awk '$1 == 100003 && $2 == 4 && $3 == "tcp" && $4 == 2049 { found = 1 } END { exit !found }'; then
    echo 'NFSv4 TCP/2049 is not registered with local rpcbind; the full Rust NFS suite would fail.' >&2
    echo 'Start a Ganesha server with rpcbind available, or use --managed-nfs on a host with free TCP/2049.' >&2
    return 1
  fi
  # Most vnfs tests can use a direct endpoint, but a few hard-code a
  # localhost/rpcbind connection; the preflight above covers those.
  export VNFS_TEST_HOST=${VNFS_TEST_HOST:-${VFSI_NFS_SERVER}:2049}
  prepare_python
  for minor in 1 2; do
    export VNFS_TEST_MINOR=$minor VFSI_NFS_MINOR=$minor
    cargo test -p vnfs --features "test-faults rpcsec-gss" --test nfs
    cargo test -p vfsi-nfs --features "server-copy test-faults" --test nfs -- --test-threads=1
    VFSI_NFS_EXPORT=${VFSI_NFS_EXPORT:-/} cargo test -p vnfs --test canonical_examples canonical_workflows_on_nfsv41_and_nfsv42 -- --ignored --test-threads=1
    "$python_bin" -m pytest \
      adapters/nfs4fs/tests/test_nfs_integration.py \
      adapters/nfs4fs/tests/test_behavior_parity.py \
      adapters/nfs4fs/tests/test_fsspec_abstract.py
  done
}

run_smb() {
  ((managed_smb == 0)) || start_smb
  : "${VFSI_SMB_SERVER:?Set VFSI_SMB_SERVER or use --managed-smb}"
  : "${VFSI_SMB_SHARE:?Set VFSI_SMB_SHARE or use --managed-smb}"
  export VFSI_SMB_REQUIRED=1
  cargo test -p vfsi-smb --features test-faults --test smb -- --test-threads=1
  prepare_smb_python
  "$python_bin" -m pytest adapters/vsmb/tests adapters/vsmbfs/tests
}

for job in check smoke fast python nfs smb uring; do
  if [[ -v selected[$job] ]]; then
    echo "==> Running local CI job: $job"
    started=$SECONDS
    "run_$job"
    echo "==> $job completed in $((SECONDS - started)) seconds"
  fi
done
