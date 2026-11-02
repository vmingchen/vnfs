# Frozen `du` traversal result

Measured November 1, 2026, on one shared 4-vCPU ARM64 Linux VM. The client
baseline is the coordinated **vnfs 0.0.27** release, not the latest moving
dependency graph. Both paths use the same release-built uutils `du` binary
from port commit `3dc073281b1465b479c4f5bd863099b446835064`: ordinary kernel
NFS traversal with `VNFS_IMPL=off`, and direct VFSI traversal with `VNFS_IMPL=nfs`.

| Network | Measured RTT, ms | Kernel median, ms | VFSI median, ms | Speedup | Kernel RPCs | VFSI RPCs | Parity |
|---|---:|---:|---:|---:|---:|---:|---|
| native | 0.053 | 1150.498 | 948.190 | 1.21× | 4,667 | 3,122 | Identical |
| added-1ms-rtt | 1.160 | 5872.339 | 4101.584 | 1.43× | 4,669 | 3,122 | Identical |

Wall times are medians of **seven capture-free invocations per method per
network**, paired with alternating order. RPC values are medians of **three
separate captured invocations**, counted from decoded wire CALL PDUs rather
than TCP packets or theoretical batch sizes. The delayed kernel samples span
4,668–4,669 calls; all other RPC counts are constant. Capture timings are
retained in the raw data but excluded from the headline wall-time medians.

VFSI used about **33% fewer RPCs** in this fixture. Both paths issued 1,555
READDIR operations. A representative native capture contains 3,112 GETATTR
operations for the kernel path and six for VFSI; VFSI also performs LOOKUP
operations within its compounds. The kernel already obtains file metadata
through READDIR, so this is not a comparison against one RPC per file.

## Fixture and controls

- **1,555 directories and 12,440 regular files**, branching factor six,
  depth four with the root at depth zero, and eight 128-byte files in every
  directory. Logical file data: 1,592,320 bytes; observed total allocation:
  57,323,520 bytes. No sparse files, hard-link aliases, symlinks or nested mounts.
- One unchanged, quiescent tree for every command: `du -a -B1 tree`.
  All **44 invocations** (warmups, timed and captured) returned identical
  sorted output: 13,995 lines. Fixture identity was checked before each run
  and verified again after collection.
- NFSv4.2 over TCP, AUTH_SYS, the VM's existing NFS-Ganesha 15.3 VFS export.
  The raw report records the running server executable/config digests, kernel,
  toolchain, binary digest, mount options and exact frozen package versions.
- Fresh client mount/superblock per invocation, with normal attribute caching
  enabled and warm server metadata. Mounting, CWD preparation, server warming,
  sorting and decoding are outside timing. VFSI connection/session setup and
  process startup are included. Kernel mount setup is excluded.
- Native is an isolated local veth with no added delay. The controlled case
  adds 500 µs to each direction, **1 ms added RTT**, rather than claiming an
  exact 1 ms total network latency. Ping measured the RTT shown above.
- Every capture has zero kernel capture drops and matched request/reply keys;
  no repeated RPC XIDs or retransmitted RPC frames were observed. The capture
  bracket starts after mount/CWD preparation and includes a 150 ms post-exit
  reply drain; namespace-local kernel housekeeping in that bracket is counted.
  Positive VFSI telemetry is required, preventing silent fallback from passing.

These results describe fresh client metadata on a directory-heavy tree with a
warm server. They do not establish warm-kernel-cache performance, WAN behavior,
concurrent-mutation semantics, or a general speedup for every filesystem shape.
The comparison is against the uutils kernel path, **not GNU `du`**.

## Reproduce and audit

The [benchmark directory](../benchmarks/du/README.md) contains the bounded
generator, isolated runner, frozen Cargo.lock and tests. The runner never follows
package patches automatically; a new baseline requires an explicit new experiment.

[Raw results](benchmarks/du-2026-11-01/results.json) retain every timing,
diagnostic, configuration and provenance record. The adjacent `raw/` directory
contains compressed stdout/stderr, original pcaps, decoded RPC fields and drop
logs. Regenerate this table while verifying the retained evidence:

```sh
python3 benchmarks/du/report.py docs/benchmarks/du-2026-11-01/results.json
```

Preliminary collection attempts were rejected for a reserved-client-port
dissector collision and a capture-buffer overrun. The complete retained set was
rerun with explicit RPC decoding, immediate capture and a 16-MiB buffer. Those
rejected attempts contribute no samples to this table.

See the [follow-up traversal profile](du-profile.md) for why this release
reaches 1.43× at added RTT and which bottlenecks remain.

The [batched development candidate](du-batching.md) is a separate comparison;
the frozen release table above remains unchanged.
