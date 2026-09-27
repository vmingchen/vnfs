# nfs4fs benchmarks

These drivers compare nfs4fs's direct NFSv4 client with fsspec's
`LocalFileSystem` on a Linux kernel NFS mount of the **same export**. They
report JSON timings and nfs4fs's reset-on-read compound/RPC counters; they do
not assert a speedup or modify network latency.

Run on a dedicated benchmark host with an NFSv4.1 or NFSv4.2 server, a mounted
copy of its export, and `nfs4fs` plus `fsspec` installed. The examples below
use a server-local export at `/srv/vnfs-ci`, mounted at `/mnt/nfs`. Replace
those paths with your own. Use `host:port` if your server has no rpcbind
registration. A unique temporary directory is created for each run and
removed afterward. The scripts first write a probe and verify that both
clients see the same export; an incorrect mapping fails before measurement.

```sh
python adapters/nfs4fs/benchmarks/small_files.py \
  --host 127.0.0.1:2049 --direct-root /srv/vnfs-ci --mount-root /mnt/nfs \
  --files 128 --bytes 4096 --rounds 10

python adapters/nfs4fs/benchmarks/directories.py \
  --host 127.0.0.1:2049 --fixture-root /srv/vnfs-ci --mount-root /mnt/nfs \
  --directories 64 --files-per-dir 8 --rounds 10

python adapters/nfs4fs/benchmarks/large_file.py \
  --host 127.0.0.1:2049 --fixture-root /srv/vnfs-ci --mount-root /mnt/nfs \
  --size-mib 256 --chunk-kib 1024 --rounds 5 --warmups 1 \
  --pipeline-workers 3 --in-flight 8
```

`--remote-root` applies the same subdirectory prefix to the direct client and
both filesystem paths. The `--fixture-root` option is optional for the new
drivers; without it, fixtures are made through `--mount-root`. Using a
server-local fixture root avoids priming the kernel NFS client's data cache
during setup, when that path is available.

| Driver | Timed operations | Validation |
| --- | --- | --- |
| `small_files.py` | Bulk `pipe` and `cat` across many small files | Every returned payload |
| `directories.py` | `find(withdirs=True)` and recursive `rm` across a multi-directory tree | File count, byte total, and removal |
| `large_file.py` | One sequential open/read/close using bounded chunks (`cache_type="none"` for nfs4fs) | Byte count and BLAKE2b digest |

`small_files.py --batch-size` varies the maximum files sent to one nfs4fs
batch; its default remains the package default (128). The large-file driver's
optional `--pipeline-workers` adds a third measurement using
`Nfs4FileSystem.read_stream_pipelined()` and independent native sessions.
`--in-flight` bounds outstanding chunks, further capped at one per worker; the
driver enforces a 16 MiB read-ahead budget. Its ordinary nfs4fs and kernel
measurements remain in the report for comparison. Pipelining is opt-in and
does not change `fs.open()`.

Setup, export checks, and optional warm-ups are excluded from timing. Each
driver alternates client order across rounds and reports medians. The
small-file driver uses fresh paths by default; `--reuse-paths` adds an untimed
warm-up over the same paths. The directory driver makes fresh trees per round.
The large-file driver reads one identical file through both clients each
round. `--warmups 0` skips application warm-up but does **not** guarantee cold
kernel or server caches. These numbers describe the specified workload and
cache state, not an intrinsic protocol speedup. Record the server, mount
options, client versions, RTT, and CPU configuration alongside published
results.

## Reproducing the latency sweep

On a **dedicated Linux VM only**, `tc netem` can add one-way delay to the
loopback interface. It affects all loopback traffic while active, including
unrelated services. Verify `tc qdisc show dev lo` is unmodified first, and
use a shell trap so the qdisc is removed even if a benchmark fails:

```sh
set -e
sudo tc qdisc add dev lo root netem delay 2.5ms # approximately 5 ms RTT
trap 'sudo tc qdisc del dev lo root' EXIT
# Run the commands above with your workload parameters.
```

Use `delay 0.5ms` for approximately 1 ms RTT, and run without a qdisc for the
baseline. Do not apply this to a production interface or overwrite an
existing qdisc. Record cache state: repeated reads of the same large file
warm the kernel page cache, so the kernel result may involve no network I/O.
The direct nfs4fs reads still issue RPCs. The large-file comparison therefore
measures warm-file application behavior, not equal amounts of wire traffic.

On the development VM (local NFSv4.2 export, 64 MiB file, 1 MiB chunks,
three rounds and one warm-up), the observed median times were:

| Approx. RTT | nfs4fs sequential | nfs4fs pipelined | Kernel NFS, warm |
| --- | ---: | ---: | ---: |
| Baseline loopback | 200 ms | 109 ms (3 workers) | 88 ms |
| 1 ms | 250 ms | 103 ms (3 workers) | 84 ms |
| 5 ms | 607 ms | 262 ms (3 workers) | 99 ms |

More workers were not always better: at 5 ms, 8 and 16 workers took 512 and
875 ms respectively, largely because each extra worker opens and closes its
own NFS descriptor. For 128 fresh 4 KiB files at 5 ms, setting
`--batch-size 16` used 8 write RPCs and a 294 ms median, versus about 47 RPCs
and 812 ms with batches of 32. The larger batches hit this server's resource
limit and fell back to extra work. These are tuning observations for this
server, **not** general defaults or guaranteed speedups; rerun on the target
server before changing application settings.
