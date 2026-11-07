# Bounded new-destination copy prototype

Keep the prototype in test support. It helps small-file NFS copying on this
fixture, but does not establish a general production improvement: large-file
NFS was slower despite fewer RPCs, and SMB used more commands. Existing
production overwrite, alias, creation and recovery behavior is unchanged.

The prototype opens at most eight source/destination pairs (16 handles),
uses at most 1 MiB of application read-buffer requests per wave, and uses
the existing vector read/write/setattr hooks. It completes acknowledged short
writes using only their unwritten suffixes and maps active indices back to
the original pair. Transport failures are returned without replaying writes.
Each owned descriptor receives one close attempt, including after data errors.

All destinations are exclusively created, proving they cannot alias retained
sources. This is narrower than the production copy API, which overwrites
existing targets and supports alias-sensitive extents. Sources in this
experiment are stable regular files with finite explicit lengths.
The NFS native writes request FILE_SYNC in both variants. SMB uses its
existing flush policy; the prototype requests SyncMode::Data at finalization.

The small case has 32 files of 4 KiB. The large case has eight files of 2 MiB.
Three paired samples alternate method order. Setup, cleanup and byte-for-byte
verification are excluded from timing; protocol counters include copy cleanup.
Every measured output matched its source, and every owned descriptor closed.

| Backend | Case | Method | Requests | Median ms |
|---|---|---|---:|---:|
| NFSv4.1 | large | existing | 152 | 159.473 |
| NFSv4.1 | large | prototype | 52 | 307.267 |
| NFSv4.1 | small | existing | 544 | 212.766 |
| NFSv4.1 | small | prototype | 88 | 84.381 |
| NFSv4.2 | large | existing | 152 | 129.971 |
| NFSv4.2 | large | prototype | 52 | 296.433 |
| NFSv4.2 | small | existing | 544 | 210.049 |
| NFSv4.2 | small | prototype | 88 | 79.206 |
| SMB | large | existing | 80 | 51.835 |
| SMB | large | prototype | 560 | 203.273 |
| SMB | small | existing | 256 | 79.219 |
| SMB | small | prototype | 320 | 80.148 |

NFS request counts are COMPOUND attempts; SMB counts are command MessageIds
from the dependency’s client diagnostics and include compound suboperations.
They are different metrics and must not be compared across protocols.
The NFS non-OK counter includes expected NOENT probes during creation; it is
not a count of failed overall copies. All measured calls completed successfully.

Timings use unoptimized Rust test builds at native latency. They do not prove
release performance or performance under added latency. A production selector
would need a broader workload comparison, existing-target and alias semantics,
and protocol-aware window sizing before enabling this path.

Coverage adds deterministic short-read/short-write, zero-progress, malformed
result, and lost-write-reply cases; a local backend test checks finite offsets,
zero-length output sizing, existing-target refusal and descriptor cleanup.
The private ignored live tests provide NFSv4.1/v4.2 and managed-SMB comparisons.

[All 36 samples](../benchmarks/copy/new-destinations.json).
Implementation lives under vfsi-sync/test_support; no application or C API
method or default is added.
