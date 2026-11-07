# Rsync retained-cursor verification — November 6, 2026

Retained-cursor integration is already present in rsync commit 0f5890fb.
The current port resolves both optional symbols through vfsi_dynamic_load,
uses them only as a complete pair, and transfers child cursors to sender
records only after ordinary rsync filtering admits a directory. Unadmitted
cursors are released with the active listing; completed and abandoned
frontier records release their remaining cursors.

This follow-up adds two regression tests covering each missing optional
symbol, with and without legacy listing support, in incremental and
non-incremental traversal. All 11 compatibility tests passed. The real
published and development SDKs each passed support/test-vfsi-port.sh.
Production rsync-vfsi.c and flist.c match the measured committed source.

## Fresh measurements

| SDK | Network | Ordinary median ms | VFSI median ms | Ordinary/VFSI time | Wire RPCs ordinary / VFSI |
|---|---|---:|---:|---:|---:|
| published | native | 1146.500 | 1702.100 | 0.67x | 4,668 / 6,231 |
| published | added-1ms-rtt | 5978.882 | 7829.443 | 0.76x | 4,668 / 6,231 |
| cursors | native | 1269.449 | 266.725 | 4.76x | 4,668 / 273 |
| cursors | added-1ms-rtt | 5886.276 | 568.480 | 10.35x | 4,668 / 273 |

Both runs use the existing fixture, dry-run sender scans, seven alternating
timing pairs, one warmup pair and three separate capture pairs per network.
The added-latency regime applies 500 microseconds in each direction on owned
veth devices. Timings exclude capture and mount preparation. This measures
warm-server NFSv4.2 AUTH_SYS metadata scanning, not transfer throughput.

Each run passed all 46 output-parity, line-count and fallback checks, and all
12 capture hashes and capture-drop checks. Every cursor-accelerated trial
used 1,554 retained cursors (22 trials). The fixture fingerprint and server
binary/configuration stayed unchanged.

The frozen published run uses port 5f211b8b9c78b682782953f4306d48122eb3ee7b
and the checksum-locked vfsi-c 0.3.12 / vnfs 0.0.28 release. Its original
baseline inputs and historical evidence are preserved. It still sends more
RPCs and is slower than ordinary rsync.

The cursor run uses port 43e55130539430092b74194882e56d948bd5db9a
and SDK commit da089db925ea9c866971f2df047e3606203ef385, without a source patch.
It uses the same workload, worker and capture protocol through development.py.
The port's uncommitted changes are tests and documentation; measured
production source equality was verified.

[Published results](benchmarks/rsync-cursors-2026-11-06/published/results.json),
[published audit](benchmarks/rsync-cursors-2026-11-06/published/audit.json),
[cursor results](benchmarks/rsync-cursors-2026-11-06/cursors/results.json),
[cursor audit](benchmarks/rsync-cursors-2026-11-06/cursors/audit.json).
Each evidence directory retains compressed outputs, traces, captures, decoded
RPC fields and its build log. Reproduction commands are in
[the frozen benchmark](rsync-benchmark.md) and
[the development runner](rsync-development.md); use the commits above,
no SDK patch for this cursor run, and a new output directory.
