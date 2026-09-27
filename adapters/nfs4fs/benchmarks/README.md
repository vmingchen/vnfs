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
  --size-mib 256 --chunk-kib 1024 --rounds 5 --warmups 1
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
