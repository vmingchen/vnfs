# READDIR child-filehandle experiment — November 5, 2026

Keep the production parent-and-name child seeds. Returned filehandles remove
LOOKUP/GETFH operations, but increase response payloads for every entry.
With the C SDK stat mask, the file-dense fixture needed 23 COMPOUNDs instead
of 18. The directory-dense fixture saved one COMPOUND only when the returned
handle cohort also grew from 24 to 32 directories. These results do not
justify changing the shipping default or adding a public policy option.

## Scope and method

This is a development protocol experiment, separate from the
[frozen published rsync benchmark](rsync-benchmark.md). The published SDK pins,
benchmark numbers, C ABI v3, production READDIR masks, and rsync cohort width
are unchanged.

The ignored private test uses the existing loopback NFS fixture with AUTH_SYS,
once for NFSv4.1 and once for NFSv4.2. Each scan enumerates the root and all
child directories to EOF, using the same 32 KiB per-READDIR reply cap.
The common root filehandle is resolved before timing. Fixture creation,
connection setup/teardown, and kernel-mounted fixture writes are excluded.

Three variants run six times per fixture and attribute profile, alternating
forward/reverse order after warming all compound planners and server data:

- Parent-and-name seeds, cohorts of 24: PUTFH/LOOKUP/GETFH/READDIR per child.
- Returned child filehandles, cohorts of 24: PUTFH/READDIR per child.
- Returned child filehandles, cohorts of 32: the same operations with wider cohorts.

The directory-dense fixture has 64 directories, each with eight regular files
(576 entries). The file-dense fixture has four such directories plus 4,096
regular files at the root (4,132 entries). Both minimal TYPE-only attributes
and the C SDK default stat/listing attribute mask are tested. The stat mask
is asserted against the real NFS attribute selector. FILEHANDLE is requested
for every entry in the candidates, including regular files, but only directory
handles are retained.

All 144 samples completed with identical names/types, no duplicate entries,
and no transport or NFS errors. Counters were identical within each six-sample
group. This tests enumeration parity, not concurrent rename/replacement safety,
full SDK attribute projection, rsync filtering, or end-to-end transfer behavior.

## C SDK stat-mask results

Times are native-loopback medians from an unoptimized Rust test harness.
They describe these samples; the deterministic operation/payload counts are
the stronger evidence. No latency was injected and no release rsync binary
was timed.

| Protocol | Fixture | Seed / cohort | RPC attempts | NFS ops | Argument bytes | Attribute bytes | Median ms |
|---|---|---|---:|---:|---:|---:|---:|
| 4.1 | directory dense | parent name / 24 | 4 | 262 | 6,736 | 50,688 | 2.083 |
| 4.1 | directory dense | returned fh / 24 | 4 | 134 | 5,456 | 71,424 | 2.131 |
| 4.1 | directory dense | returned fh / 32 | 3 | 133 | 5,392 | 71,424 | 1.762 |
| 4.1 | file dense | parent name / 24 | 18 | 68 | 2,912 | 363,616 | 11.748 |
| 4.1 | file dense | returned fh / 24 | 23 | 75 | 3,552 | 512,368 | 13.586 |
| 4.1 | file dense | returned fh / 32 | 23 | 75 | 3,552 | 512,368 | 13.255 |
| 4.2 | directory dense | parent name / 24 | 4 | 262 | 6,736 | 50,688 | 2.144 |
| 4.2 | directory dense | returned fh / 24 | 4 | 134 | 5,456 | 71,424 | 2.067 |
| 4.2 | directory dense | returned fh / 32 | 3 | 133 | 5,392 | 71,424 | 1.790 |
| 4.2 | file dense | parent name / 24 | 18 | 68 | 2,912 | 363,616 | 14.193 |
| 4.2 | file dense | returned fh / 24 | 23 | 75 | 3,552 | 512,368 | 16.294 |
| 4.2 | file dense | returned fh / 32 | 23 | 75 | 3,552 | 512,368 | 15.655 |

Each returned filehandle was 32 bytes on this server. Its length prefix and
opaque bytes add 36 attribute bytes per entry: 20,736 extra bytes for the
directory-dense tree and 148,752 for the file-dense tree. Stat attributes grew
from 88 to 124 bytes per entry, about 41%. In the file-dense case, those bytes
also required five more READDIR RPCs, outweighing the eight saved lookup/getfh
operations.

At cohort 24 the directory-dense RPC count stayed at four: fewer operations
did not eliminate a round trip. Cohort 32 reduced it to three. A larger
parent-and-name cohort is a separate experiment and was not measured here;
the wider candidate is not evidence for a filehandle-only speedup.

The TYPE-only profile, retained in the raw samples, needed 14 RPCs for both
file-dense variants. It would have hidden the extra pagination caused by the
real stat mask. Requesting a realistic attribute set matters.

RPC attempts come from client-scoped COMPOUND telemetry, not a packet capture.
Argument bytes exclude RPC, authentication, and TCP framing. Attribute bytes
count only raw entry attribute payloads, excluding names, entry links,
cookies, bitmaps, and other reply fields. Neither byte column is total wire
traffic. Timings include the harness name/type collection but no C callbacks.

## Reproduce

The mount must correspond to the root of the explicitly configured server
export. The test creates exclusive task-owned fixture directories and removes
only those directories. An absent fixture or omitted FILEHANDLE fails the
test; it does not silently fall back.

```sh
VFSI_NFS_SERVER=127.0.0.1 VFSI_NFS_TEST_MOUNT=/mnt/nfs VFSI_NFS_MINOR=2 \
  cargo test -p vfsi-nfs --lib --all-features \
    nfs::directory_benchmark::compare_returned_filehandles_to_parent_name_seeds \
    -- --ignored --exact --nocapture
```

Repeat with minor 1. Each successful run emits 72 JSON sample lines prefixed
with `READDIR_HANDLES`. The complete 144-sample run and measurement
metadata are in [native.json](../benchmarks/nfs-directory-handles/native.json).
The harness is private and compiled only for tests; it introduces no public
API, production environment switch, or runtime overhead.

If revisited, measure actual filtered rsync trees, added latency, other server
filehandle sizes/response budgets, and retained-identity behavior before
choosing any adaptive policy. Keep that work separate from published-baseline
rsync measurements.
