#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

# Keep this list explicit: several published packages need different feature
# sets, and the live NFS/SMB integration suites run in their dedicated jobs.
# Enable deterministic fault injection in the fast suites too; otherwise
# cfg(feature = "test-faults") regressions are silently omitted.
cargo test -p vfsi-core --features test-faults
cargo test -p vfsi-sync --features "test-faults test-support"
cargo test -p vfsi-local --features test-faults
cargo test -p vfsi-nfs --lib --all-features
cargo test -p vfsi-smb --lib
cargo test -p nfsv41-sys
./scripts/test-libntirpc.sh
cargo test -p vnfs --features test-faults --lib
cargo test -p vnfs --features "dummy test-faults" --test dummy_vecfs
cargo test -p vnfs --test public_api
cargo test -p vnfs --test application_boundary
cargo test -p vnfs --test tree_builder
cargo test -p vnfs --doc
cargo test -p vfsi-c --lib

# Python extension crates are intentionally detached Cargo workspaces so their
# maturin distributions retain independent lockfiles. Test them explicitly so
# a successful root-workspace run cannot accidentally omit their Rust code.
cargo test --manifest-path adapters/vfsi-python/Cargo.toml --locked --all-features
cargo test --manifest-path adapters/nfs4fs/Cargo.toml --locked
cargo test --manifest-path adapters/vsmb/Cargo.toml --locked
