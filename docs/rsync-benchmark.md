# Frozen rsync sender-scan baseline — November 2, 2026

This measures the published **vnfs 0.0.28 / vfsi-c 0.3.12 (ABI v3)** release
set, using rsync port commit `5f211b8b9c78b682782953f4306d48122eb3ee7b`.
It does not follow subsequent SDK patches or use the development directory-cohort
change. The pinned archive checksum, complete SDK lockfile and expected package
versions live in [the benchmark baseline](../benchmarks/rsync/baseline.json).

| Network | Ordinary rsync median | VFSI rsync median | VFSI / ordinary time | Wire RPCs: ordinary / VFSI |
|---|---:|---:|---:|---:|
| Native, measured 0.058 ms RTT | 1,166.782 ms | 1,727.409 ms | **1.48× slower** | 4,668 / 6,231 |
| +1 ms RTT, measured 1.100 ms | 5,776.390 ms | 7,720.686 ms | **1.34× slower** | 4,668 / 6,231 |

These are seven uninstrumented timing pairs per network, with alternating order,
one warmup pair and three separate capture pairs. All captures of a given method
have the same RPC count. Timings exclude mount setup and capture overhead, and
include process startup and VFSI connection setup. Packet captures include actual
NFS RPC PDUs, including connection setup/teardown where performed during the
command; they are not packet counts or client-side estimates.

## Fixture and correctness

Both methods run the same VFSI-enabled rsync binary with `-ani
--out-format='%i %n%L' tree/ destination/`, with the adapter disabled or enabled.
Each run has a new empty local destination and a fresh NFSv4.2 AUTH_SYS mount
(`nosharecache`, ordinary kernel attribute-cache defaults). Server data is warm;
this is a metadata scan, not a cold-file-data or transfer-throughput benchmark.
The owned veth namespace adds 500 µs in each direction for the delayed case.
No existing interface or host-wide qdisc is changed.

The shared `du` fixture contains 1,555 directories and 12,440 regular files:
branching factor 6, depth 4, eight 128-byte files per directory. Logical file
content totals 1,592,320 bytes; allocated space is 57,323,520 bytes. It contains
no symlinks or hard-link aliases. Its metadata/identity fingerprint is checked
before every run and after the experiment.

All **46 runs** produced exactly the same 13,995 output lines, byte for byte.
Every accelerated run had positive `VFSI_TRACE` entry counts. A subsequent
[diagnostic audit](benchmarks/rsync-2026-11-02/diagnostic-audit.json) verifies
that all 22 accelerated traces contain only successful listing records, with no
partial-POSIX-fallback warning; the runner rejects mixed success/fallback traces. A missing shared
library and unsupported `--hard-links` each retained ordinary traversal, produced
identical output and emitted no successful-acceleration trace. Every dry-run
left its destination empty. Every capture had zero kernel drops and matched RPC
request/reply keys. These checks do not establish behavior under arbitrary
concurrent mutation or equivalence for every rsync option.

## What the result tells us

VFSI is slower on this released rsync path. The request tags in one native
capture contain **1,555 each** of `resolve`, `getattr`, `lookup_typev` and
`readdir_pages`: four compounds per directory, plus session/setup operations.
Ordinary rsync uses 4,668 RPCs, approximately three per directory. This accounts
for the delayed result closely: VFSI sends 33.5% more RPCs and takes 33.7% longer.
Positive tracing alone therefore does not prove useful batching.

This sender-scan path does not receive the retained recursive traversal benefit
measured by the current `du` development benchmark. Removing repeated resolution
and carrying directory cursors across rsync's sender frontier is the next
performance experiment; it should be compared with this unchanged release
baseline, rather than replacing the baseline with an unmeasured patch.

## Reproduce and inspect

```sh
# In the exact rsync port checkout, bootstrap the checksum-pinned SDK once:
support/build-vfsi-c

# From the vNFS repo; generate only into a NEW exported directory, or reuse
# the existing du fixture without modifying it:
python3 benchmarks/rsync/generate.py /export/new-rsync-fixture
CARGO_BUILD_JOBS=2 python3 benchmarks/rsync/run.py run \
  --checkout /path/to/frozen-rsync-port \
  --fixture /export/new-rsync-fixture --export /new-rsync-fixture \
  --output /path/to/new-results --pairs 7 --captures 3
python3 -m unittest discover -s benchmarks/rsync
```

The runner rebuilds the verified release SDK using the checked-in lock and
rejects port-source edits, manifest drift, non-registry dependencies, archive
source edits, output mismatch, missing acceleration, false fallback or capture
loss. Documentation-only edits to `VFSI.md` are permitted and recorded.
Linux, passwordless sudo for isolated namespaces/mounts, NFS client tools,
iproute2, tcpdump and tshark are required. Do not run competing benchmarks
concurrently.

[Raw results](benchmarks/rsync-2026-11-02/results.json) retain all samples, fixture
geometry, calibrated latency, server/config hashes, tool versions, resolved
packages, binary/library hashes, build log, per-run output/trace and compressed
wire captures. [Compound tags](benchmarks/rsync-2026-11-02/compound-tags.json)
provide the request-shape breakdown above. For example, decode a capture with
`gzip -dc CAPTURE.pcap.gz | tshark -n -r - -d tcp.port==2049,rpc -Y
'rpc.msgtyp == 0 && rpc.program == 100003 && rpc.procedure == 1' -T fields
-e nfs.tag`.
