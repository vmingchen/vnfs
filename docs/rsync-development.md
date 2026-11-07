# Development rsync retained-cohort traversal

This is a separate development measurement. The published vnfs 0.0.28 /
vfsi-c 0.3.12 benchmark, lockfile, checksums and report remain unchanged.

The VFSI and ordinary sender use the same isolated release-built rsync binary.
Both use dry-run (-ani) on the existing fixture, with fresh private NFSv4.2
AUTH_SYS mounts and empty local destinations. Source file data is not copied.
Only the benchmark’s newly created veth devices receive added latency.

## Results

| Network | Ordinary median ms | VFSI median ms | Speedup | Wire RPCs ordinary / VFSI |
|---|---:|---:|---:|---:|
| native, measured RTT 0.069 ms | 1117.132 | 263.379 | 4.24× | 4,668 / 273 |
| added-1ms-rtt, measured RTT 1.170 ms | 5807.011 | 537.927 | 10.80× | 4,668 / 273 |

Each network has one warmup pair, seven uninstrumented timing pairs with
alternating order, and three separate capture pairs. Captures are excluded
from the reported timing medians. All capture repetitions had the same count.
These are decoded RPC calls rather than packet counts; matched replies and
capture-drop checks passed. The count includes application connection setup
and teardown, while mount preparation is outside the measured interval.

All 46 trials passed output parity and line-count checks. VFSI trials required
positive cohort-mode traces; missing-library and unsupported-option trials
verified ordinary fallback without claiming acceleration. The fixture has
1,555 directories and 12,440 regular files and retained its original fingerprint.
The source fixture, measured binaries, library, and measurement scripts were
verified unchanged. Server binary/configuration hashes also match the record.

The results preserve their measurement-time script hashes and SDK patch.
Subsequent runner fixes isolate patch application from enclosing repositories
and require positive aggregate cursor reuse. All 22 archived accelerated traces
pass the stronger gate; the recorded timings and captures are unchanged.

## Inputs and scope

- SDK base commit: `93a1e1213dbb27fc5c7f0d77c3458dc46ef2a4c4`.
- SDK candidate patch SHA-256: `995cbe7627ca7bff08a2d4aeff708bca49e9fb049131d78940874a3c4993957d`.
- Rsync port commit: `43e55130539430092b74194882e56d948bd5db9a`.
- The candidate SDK includes the merged directory-cohort implementation and
  this worktree’s explicit patch. The copy prototype and protocol experiments
  are test-only and are absent from the release execution path.
- The cursor-aware path is exercised here. The separate legacy C allocation
  result must not be treated as the cause of this end-to-end improvement.
- This is a warm-server metadata scan on one synthetic fixture, two latency
  settings and AUTH_SYS. It does not establish transfer throughput, cold-cache,
  Kerberos, SMB, filtered-tree, or general WAN performance.

The archived candidate patch is gzip-compressed; its recorded SHA-256 is for
the decompressed bytes.

The development runner archives exact full commit hashes, applies an explicit
SDK patch to its private snapshot, and records source archive/patch, binaries,
library, lockfile, resolved versions, build environment and script hashes.
The input repositories and their build trees are not reconfigured by the run.
The shared worker/orchestrator preserves the published runner’s default
revision/archive/registry and trace gates; development has a separate CLI.

## Reproduce

```sh
gzip -dc docs/benchmarks/rsync-development-2026-11-05/candidate-sdk.patch.gz > /tmp/vnfs-sdk-candidate.patch
python3 benchmarks/rsync/development.py run \
  --checkout /path/to/vfsi-port-rsync \
  --port-revision 43e55130539430092b74194882e56d948bd5db9a \
  --sdk-checkout /path/to/vnfs \
  --sdk-revision 93a1e1213dbb27fc5c7f0d77c3458dc46ef2a4c4 \
  --sdk-patch /tmp/vnfs-sdk-candidate.patch \
  --target-dir /path/to/separate-release-cache \
  --fixture /srv/vnfs-ci/vfsi-du-v027-20261101 --export /vfsi-du-v027-20261101 \
  --output /path/to/new-results --pairs 7 --captures 3
```

The output directory must be new. The SDK patch is explicit; current input
worktree edits are not silently included in an exact-revision snapshot.
Linux, passwordless sudo for private mount/network namespaces, NFS tools,
iproute2, tcpdump and tshark are required. Run without competing benchmarks.
The helper refuses an occupied benchmark subnet and removes only owned resources.

[Results and full provenance](benchmarks/rsync-development-2026-11-05/results.json),
[raw output/traces and captures](benchmarks/rsync-development-2026-11-05/raw),
[candidate SDK patch](benchmarks/rsync-development-2026-11-05/candidate-sdk.patch.gz),
[build log](benchmarks/rsync-development-2026-11-05/build.log.gz).
The original full build snapshots remain at `/tmp/vnfs02-rsync-development.oH0G8OMm/run` on the VM.
