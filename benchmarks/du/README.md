# Frozen `du` traversal benchmark

This benchmark stays on the coordinated **vnfs 0.0.27** release. It does not
read `ecosystem/versions.toml`, follow a moving branch, or update dependencies.
`baseline.json` fixes the port commit, library versions and Cargo.lock digest;
the checked-in lock fixes every transitive registry checksum. Changing the
baseline is an explicit, separately named experiment. Preserve earlier results.

The comparison uses the **same release-built uutils binary**:
`VNFS_IMPL=off` exercises its ordinary kernel NFS traversal; `VNFS_IMPL=nfs`
uses VFSI. It is not a comparison against GNU `du`. No GNU source or fixtures
are used.

## Reproduce on a Linux NFS lab server

Required tools: Python 3.11+, Cargo/Rust compatible with the frozen port,
iproute2, nfs-common, ping, tcpdump, tshark, and passwordless `sudo` for
namespace/mount/capture setup. Install the native Rust build dependencies
documented by vNFS as well. An existing NFSv4.2 AUTH_SYS Ganesha server must
listen on TCP port 2049 on all addresses and export the fixture parent.
The runner never changes the server configuration, global cache state, or
an existing interface's qdisc.

For the published lab, `/srv/vnfs-ci` is exported as `/`. From the vNFS root:

```sh
git -C ~/gh/vfsi-port-coreutils worktree add --detach ~/vfsi-du-baseline-0.0.27 \
  3dc073281b1465b479c4f5bd863099b446835064
python3 benchmarks/du/generate.py /srv/vnfs-ci/du-v027
python3 benchmarks/du/run.py run \
  --checkout ~/vfsi-du-baseline-0.0.27 \
  --fixture /srv/vnfs-ci/du-v027 --export /du-v027 \
  --output ~/du-v027-results --pairs 7 --captures 3
```

Generation refuses existing directories and limits entries/payload bytes.
The default tree has branching factor 6, depth 4 (root at depth 0), eight
128-byte regular files in **every** directory, and no symlinks, hard-link
aliases, sparse files, or nested mounts. Files are fsynced; the manifest records
actual block allocation and an inode/metadata identity digest. The same tree
is reused throughout, with its digest checked before each command.

## Measurement contract

- Build outside measurement with `--release --locked`; validate the source
  revision, clean tracked tree, lock digest, and resolved registry packages.
  Record the binary digest, toolchain and build flags.
- A dedicated veth/network namespace uses `169.254.241.0/30`; overlapping host
  routes cause failure. Each invocation gets a private mount namespace and a
  fresh `nosharecache` NFSv4.2 mount. Normal attribute caching remains enabled;
  there is no `noac`, `actimeo=0`, or global `drop_caches`.
- Server backing metadata is warmed outside every trial. One unmeasured pair
  also warms Ganesha before each regime. This is **fresh client metadata,
  warm server**, not an already-warm kernel-cache comparison.
- Native means no added delay over the local veth, not a physical LAN. The
  controlled regime adds 500 microseconds to each veth direction (1 ms added
  RTT). Nine ping samples record the observed RTT and both qdisc settings.
- Each command is `du -a -B1 tree`, with a local-file stdout/stderr sink.
  Mounting, CWD preparation, warming, sorting and capture decoding are outside
  timing. Process startup and VFSI connection/session setup remain included.
  Seven capture-free pairs alternate kernel/VFSI order; the table reports each
  method's median and their ratio.
- Three additional captured pairs per regime measure **actual wire RPCs**.
  Their elapsed times are retained but excluded from headline medians to
  avoid packet-capture observer overhead. `tcpdump --immediate-mode` and a
  bounded 16-MiB buffer handle short commands and scheduling bursts.
- Decode TCP port 2049 explicitly as RPC: reserved client ports can otherwise
  select an unrelated dissector. Count RPC CALL PDUs, not packets; multiple
  PDUs in one frame are counted separately. Require zero capture drops and
  matching request/reply keys. Retain repeated-XID/retransmission indicators.
  Counts include VFSI session setup/teardown and any kernel path checks during
  the command, but exclude kernel mount setup/teardown.
- Every command must exit successfully and produce the same sorted output
  digest and expected line count. VFSI must emit positive RPC telemetry;
  silent fallback invalidates the experiment. Wire counts are authoritative;
  library telemetry does not include kernel RPCs made by that process.
  The bracket includes a 150 ms post-exit reply drain; namespace-local kernel
  housekeeping in that bracket is counted too.

`results.json` retains every sample, geometry, provenance and configuration.
`raw/` retains per-command stdout/stderr, individual sample JSON, decoded RPC
fields, tcpdump drop counters, and compressed original pcaps. These are the
generated public fixture's AUTH_SYS traffic, isolated from other server users.
The fixture remains for inspection; namespace/link/mount cleanup is automatic.

Regenerate the results table and verify all retained output hashes, RPC
request/reply counts, capture-drop logs and pcap digests:

```sh
python3 benchmarks/du/report.py docs/benchmarks/du-2026-11-01/results.json
```

Run the bounded generator/accounting/safety tests with:

```sh
python3 -m unittest discover -s benchmarks/du -v
```

The tiny real source-port-705 pcap guards explicit RPC decoding. That integration
test skips when tshark is absent; the pure Python tests still run. Live network
and performance tests are explicit lab runs, not ordinary CI timing gates.

See [published results](../../docs/du-benchmark.md) for the retained run and
its limitations. New package patches do not rewrite this baseline or table.
