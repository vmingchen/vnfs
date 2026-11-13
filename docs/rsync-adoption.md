# Rsync adoption on published SDK 0.3.18

The exact frozen port passes real-transfer correctness against stock upstream
on this NFSv4.2 AUTH_SYS fixture. Initial copies and resumed transfers issue
more RPCs and take longer through VFSI; metadata-heavy updates and deletes
issue far fewer RPCs. These are captured correctness trials, not timing medians.

## Inputs and method

Measurements used the runner at commit 98fd81c with absolute preparation paths.
The evidence bundle retains that exact `runner-snapshot.py` and its recorded
SHA-256. Subsequent review fixes affect preparation path resolution, interrupted
capture startup cleanup, and audit strictness; they do not change these measured
trials or rewrite their provenance.


- Port: 7f4f8f87a58510acfcc71fc28c3461138327b848.
- Stock upstream: ea645201fefbba7f6b6b22b621856dba3c01cdd3, the port’s upstream base.
- SDK: checksum-pinned published vfsi-c 0.3.18, registry dependencies only; vfsi-core/sync/local/posix/smb 0.2.1 and vfsi-nfs 0.2.3.
- Older fallback library: vfsi-c 0.3.11, ABI v3, without extended I/O symbols. Its Rust sources match the published archive; the local cache pins published vnfs exactly to 0.0.27. The archived legacy manifest/lockfile record the graph used.
- The existing fixture has 1,555 directories and 12,440 regular files, each 128 bytes. Its identity remains 8b054ac6860c86a6c98ddf06e659262a719d758191185e70ddcf5267419a0853.
- Native measured RTT: 0.053 ms; added latency measured RTT: 1.140 ms. Only newly owned veth devices received 500 microseconds of delay per direction.

Production sources are exact Git snapshots; the published SDK archive checksum
and lockfile are verified. Neither active port checkout nor frozen benchmark
inputs are reconfigured. The existing fixture is cloned into owned directories;
all subsequent fixture mutations go through NFS to keep Ganesha coherent.
Each transfer invocation gets a fresh private NFS mount and separate client
page cache. Working-directory preparation happens before capture and timing,
matching the original scan protocol. All owned mounts, routes and namespaces
are removed. A failed unmount never permits recursive deletion of a mounted
source.

Each transfer case runs stock then VFSI once per latency regime. All reported
transfer times include capture overhead and process/connection startup; mount,
CWD and capture setup are excluded. These single ordered samples do not establish
throughput or a general speedup. Destination parity covers paths, object types,
permissions, byte sizes, SHA-256 data hashes, and symlink targets. It does not
claim additional ownership, ACL, xattr, or nanosecond timestamp coverage.

## Real-transfer results

Times are milliseconds. Each cell is stock / VFSI.

| Case | Native ms | Added-latency ms | Native RPCs | Added-latency RPCs |
|---|---:|---:|---:|---:|
| create | 11,747 / 13,248 | 54,880 / 69,666 | 41,988 / 53,141 | 41,989 / 53,143 |
| update | 1,345 / 475 | 5,986 / 635 | 4,674 / 281 | 4,674 / 281 |
| delete | 1,219 / 270 | 6,073 / 531 | 4,665 / 273 | 4,665 / 273 |
| excludes | 1,203 / 270 | 6,017 / 566 | 4,668 / 277 | 4,668 / 277 |
| symlinks | 1,168 / 298 | 5,884 / 569 | 4,676 / 287 | 4,676 / 287 |
| partial-failure | 1,395 / 452 | 6,700 / 917 | 4,674 / 283 | 4,674 / 283 |
| resumed | 11,003 / 13,364 | 55,229 / 69,609 | 41,998 / 53,150 | 41,999 / 53,152 |

Both implementations matched independently reconstructed expected destination
inventories after create, same-size updates and permission changes, deletion,
excludes, symlinks (including a dangling link), and resume. The deliberate
unreadable-file case returned rsync status 23 in both implementations, preserved
the permission diagnostic, omitted the unreadable file, and copied every healthy
sibling.

Four interrupted attempts received SIGINT after real partial bytes appeared and
returned status 20. Their retained large-file prefixes matched the known source
payload. A fresh process and mount then reconnected and completed the tree.
This proves restart correctness; local rsync may replace partial files under its
normal whole-file policy, so no delta-resume throughput claim is made. Intentional
interrupted attempts were not captured; all 36 completed transfer invocations
and four scan invocations have balanced wire captures with zero capture drops, repeated XIDs or TCP retransmission flags.

On the initial native copy, both implementations perform 12,440 OPEN, READ and
CLOSE operations. VFSI adds 12,441 GETATTR operations and 15,557 LOOKUP operations.
The captured counts show additional metadata work around file data access; the
measurement does not separately profile its cause. Initial-copy and resume
overhead requires a separate optimization, rather than applying the historical
sender-scan speedup to data transfers.

## Fallback and scan regression gates

All six fallback transfers matched their two stock controls. The real older ABI-v3
library used legacy path listing (positive mode=paths traces, zero retained
cursors). A missing library and unsupported hard-link mode used POSIX without
successful acceleration traces. Missing-library fallback required the actual
pre-dispatch load-failure diagnostic.

Both latency regimes preserved exactly 273 VFSI scan RPCs versus 4,668 stock
RPCs, with identical 13,995-line dry-run output and positive retained-cursor
traces. The 4.76× / 10.35× historical medians remain scan-only results in
[the retained-cursor verification](rsync-retained-cursor-verification.md); they
are not new transfer timings and their original evidence is unchanged.

The independent audit reconstructs every expected destination from the fixture
recipe, verifies partial prefixes and statuses, rechecks all output and capture
hashes, re-decodes all 40 captures, verifies matched RPC calls/replies, and
confirms the original fixture identity.

## Reproduce

Use Linux with passwordless sudo, an existing NFSv4.2 AUTH_SYS Ganesha export
of /srv/vnfs-ci, NFS tools, iproute2, tcpdump and tshark. The generator is the
existing benchmarks/rsync/generate.py / benchmarks/du/generate.py; the mutation
recipes and private latency lifecycle are in the adoption runner. Do not run
competing workloads during collection.

On a new test host, generate a new fixture under the existing export with
benchmarks/rsync/generate.py NEW_ROOT --branches 6 --depth 4 --files 8 --size 128.
The generator refuses existing directories. Preserve the existing fixture on this VM.

~~~sh
# Build a real older ABI-v3 fallback SDK in a separate prefix.
mkdir /tmp/rsync-legacy-source
curl --proto '=https' --tlsv1.2 -fsSL \
  https://static.crates.io/crates/vfsi-c/vfsi-c-0.3.11.crate \
  -o /tmp/rsync-legacy-source/old.crate
printf '%s  %s\n' \
  5f4a4af41f88b3f2ade313b38825c9120ae3ed2a8ded6180a058ecd6f556871b \
  /tmp/rsync-legacy-source/old.crate | sha256sum -c -
tar -xzf /tmp/rsync-legacy-source/old.crate -C /tmp/rsync-legacy-source
python3 bindings/c/build-sdk.py --registry-only --profile release \
  --manifest-path /tmp/rsync-legacy-source/vfsi-c-0.3.11/Cargo.toml \
  --target-dir /tmp/rsync-legacy-target --prefix /tmp/rsync-legacy-sdk
python3 benchmarks/rsync/adoption.py prepare \
  --checkout ~/gh/vfsi-port-rsync \
  --output /tmp/rsync-adoption-build \
  --target-dir /tmp/rsync-release-cache
python3 benchmarks/rsync/adoption.py run \
  --prepared /tmp/rsync-adoption-build \
  --fixture /srv/vnfs-ci/vfsi-du-v027-20261101 \
  --old-library /tmp/rsync-legacy-sdk/lib/libvfsi_c.so \
  --output /tmp/rsync-adoption-results
cp benchmarks/rsync/adoption.py /tmp/rsync-adoption-results/runner-snapshot.py
python3 benchmarks/rsync/audit_adoption.py \
  --results /tmp/rsync-adoption-results \
  --fixture /srv/vnfs-ci/vfsi-du-v027-20261101 \
  --output /tmp/rsync-adoption-results/audit.json
python3 -m unittest discover -s benchmarks/rsync
~~~

All output directories must be new. Build the older library from its checksum-
pinned published crate using bindings/c/build-sdk.py --registry-only; the same
0.3.11 archive and real-library behavior are required by ci/test-c-outcomes.sh.

[Results](benchmarks/rsync-adoption-2026-11-12/results.json),
[independent audit](benchmarks/rsync-adoption-2026-11-12/audit.json),
[build provenance](benchmarks/rsync-adoption-2026-11-12/provenance.json),
[fixture](benchmarks/rsync-adoption-2026-11-12/fixture.json), and
[build log](benchmarks/rsync-adoption-2026-11-12/build.log.gz).

Complete compressed captures, output, manifests, and the exact measured runner
remain in /tmp/vfsi-rsync-adoption-20261112-results5 on the VM; the associated
isolated build is /tmp/vfsi-rsync-adoption-20261112-build.

The complete local evidence package is vfsi-weekly-evidence-20261112.tar.gz
(341 individually checksummed files). It is delivered locally and is not
uploaded or published. Its SHA-256 is
efa2e3a4558226cac5c6e9b44cd7bc625c58d8348f6df3e5e1f8ef8220a3f624.
See [bundle identity](benchmarks/rsync-adoption-2026-11-12/bundle.json) and
[legacy graph provenance](benchmarks/rsync-adoption-2026-11-12/legacy-provenance.json).
