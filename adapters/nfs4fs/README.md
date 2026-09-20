# nfs4fs

`nfs4fs` is a Linux `fsspec` implementation backed by
[VFSI](https://github.com/vmingchen/vnfs)'s vectorized NFSv4 client. Bulk
`fsspec` operations are translated into NFS compounds, reducing round trips
for workloads with many small files.

## Install

```sh
python -m pip install nfs4fs
```

Published wheels use CPython's stable ABI and support standard CPython 3.9 and
newer on Linux x86-64 and AArch64. A separate CPython 3.14 free-threaded wheel
is built and tested on x86-64.

A source build additionally requires Rust, Clang, `pkg-config`, libntirpc 4.3
or newer, Kerberos/GSS development headers, and userspace-RCU development
headers. Source and editable builds link the administrator-provided system
`libntirpc` dynamically, so that shared library must be present at runtime.

Release wheels are self-contained. They are built against a checksum-pinned
libntirpc source revision, and `auditwheel` copies `libntirpc` plus its
Kerberos (`libgssapi_krb5`/`libkrb5`) and userspace-RCU (`liburcu`)
dependencies into the wheel's `.libs/` directory, rewriting RPATH so the
package imports without a system `libntirpc`. CI fails a release whose wheel
does not bundle those libraries.

The package installs `fsspec.specs` entry points for both `nfs4` and `vfsi`, so
normal use does not require `import nfs4fs` before calling `fsspec`.

## NFSv4

Pass the server as an option or as the URL authority:

```python
import fsspec

fs = fsspec.filesystem(
    "nfs4",
    host="nfs.example",
    root="exports/project",
    auto_mkdir=False,
)

fs.pipe({"/file-1": b"hello", "/file-2": b"world"})
files = fs.cat(["/file-1", "/file-2"])

with fsspec.open("nfs4://nfs.example/exports/project/file-1", "rb") as file:
    print(file.read())
```

The `pipe()` call gives both writes to VFSI at once. For these two small files,
it encodes both independent write chains in one NFSv4 COMPOUND and completes
them in one network round trip. The following `cat()` also preserves the batch;
it currently uses one compound for bounded size discovery and one for the
reads.

The client negotiates NFSv4.2 and falls back to NFSv4.1. Set
`minor_version=1` or `minor_version=2` to require a specific version. AUTH_SYS
remains the default. For Kerberos-backed RPCSEC_GSS, obtain a ticket in the
process credential cache and select `authentication="krb5"` or
`authentication="krb5i"`; the latter integrity-protects RPC payloads. Set
`require_secure_authentication=True` to fail closed if AUTH_SYS was selected.
An optional `service_principal` overrides the default `nfs@<host>` principal.

`compound_size_limit` caps the payload merged into one compound. The default is
1 MiB; lower it when a server has a smaller request limit:

```python
fs = fsspec.filesystem(
    "nfs4", host="nfs.example", compound_size_limit=256 * 1024
)
```

Connection setup and individual NFS RPCs are independently bounded. Defaults
are 10 seconds and 5 seconds, respectively:

```python
fs = fsspec.filesystem(
    "nfs4",
    host="nfs.example",
    connect_timeout=5.0,
    request_timeout=15.0,
)
```

## Operations that vectorize

The largest benefit comes from giving `fsspec` multiple paths at once:

- `cat`, `cat_ranges`, and `open_files` batch reads;
- `pipe` and `put` batch writes;
- recursive `get`, `put`, `cp`, `rm`, and tree walks use vector metadata and
  mutation operations;
- `cp_file`/`copy` retain the low-round-trip vector path for bounded small-file
  batches. Files beyond the read allocation budget use NFSv4.2 server-side
  copy when available, in bounded 16 MiB extents, with a streaming client-side
  fallback.

`minor_version()`, `capabilities()`, `server_copy_enabled()`,
`compound_stats()`, and `rpc_stats()` are available on the native client for
feature inspection and diagnostics.

## Small-file benchmark

A local benchmark compared nfs4fs directly with vanilla fsspec
`LocalFileSystem` accessing the same export through a Linux kernel NFS mount.
The server was NFS-Ganesha V15.3 over NFSv4.2 on an AArch64 Ubuntu VM, using
nfs4fs 0.3.1 and fsspec 2026.7.0. Linux `netem` added 500 microseconds to each
loopback traversal, approximately 1 ms of network round-trip delay; ping
averaged 1.43 ms including VM scheduling overhead. Each result is the median
of 30 cold-path trials over 20 files of 4 KiB, with connection setup excluded,
fresh paths used for every trial, and client execution order alternated.

| Operation | nfs4fs | Kernel NFS + `LocalFileSystem` | Speedup | nfs4fs RPCs |
| --- | ---: | ---: | ---: | ---: |
| `pipe` 20 files | 38.80 ms | 151.31 ms | 3.90x | 1 |
| `cat` 20 files | 4.16 ms | 83.19 ms | 20.01x | 2 |

The same benchmark was repeated after an untimed warm-up, reusing the exact
same files for every measured trial. Ping averaged 1.35 ms during this run:

| Warm operation | nfs4fs | Kernel NFS + `LocalFileSystem` | Speedup | nfs4fs RPCs |
| --- | ---: | ---: | ---: | ---: |
| `pipe` 20 files | 38.03 ms | 143.30 ms | 3.77x | 1 |
| `cat` 20 files | 3.65 ms | 51.94 ms | 14.25x | 2 |

The kernel's metadata and page caches reduced its repeated-read time, but a
normal NFSv4 open and close still involves server state. `LocalFileSystem`
opens the files serially, so those latency-bearing operations remain serial as
well. nfs4fs does not retain file data across separate `cat()` calls; its warm
improvement comes from server-side caches while it continues to batch the
remote operations. Repeated writes changed little because both clients still
have to send the new contents to the server.

The benchmark measures a latency-bound small-file workload, not sequential
bandwidth, and results will vary with the server, client, mount options, and
compound limits. The exact driver is
[`benchmarks/small_files.py`](https://github.com/vmingchen/vnfs/blob/main/adapters/nfs4fs/benchmarks/small_files.py);
it verifies returned data and reports VFSI's compound and RPC counters so
batching regressions are visible alongside timing changes. Pass
`--reuse-paths` to reproduce the warm-cache variant.

All data-transfer entry points accept fsspec's `callback=` argument. Bulk
operations (`cat`, `cat_ranges`, `pipe`, `get`, `put`, and `copy`) report item
progress on the parent callback and byte progress on a branched callback for
each file. Single-file operations report bytes directly, and oversized
streaming transfers update after every chunk.

## File buffering

Read handles use fsspec's standard per-open caches by default. The default
1 MiB block size matches the VFSI range-I/O path and can be changed per
filesystem or per direct `fs.open()` call:

```python
fs = fsspec.filesystem(
    "nfs4",
    host="nfs.example",
    block_size=256 * 1024,
    cache_type="readahead",
)

with fs.open("/large.parquet", "rb", cache_type="blockcache") as file:
    header = file.read(4096)
    file.seek(-8192, 2)
    footer = file.read()
```

All cache types registered by fsspec are accepted. Use `cache_type="none"`
for unbuffered reads, especially when another client can modify an already
open file. Buffered data belongs to one open handle and is discarded on close.
Blocks already fetched by that handle remain cached, but fsspec does not
revalidate the source while the handle is open, so uncached blocks may reflect
a concurrent writer. This is a per-open cache, not a persistent client cache.

fsspec also provides a persistent sparse-file wrapper under the same
`blockcache` name:

```python
cached = fsspec.filesystem(
    "blockcache",
    target_protocol="nfs4",
    target_options={"host": "nfs.example"},
    cache_storage="/var/tmp/nfs4fs-cache",
    check_files=True,
)
```

With `check_files=True`, each open compares the saved source identity with
nfs4fs's `ukey()` and starts a fresh local cache generation when it changes.
For NFS this identity uses `FATTR4_CHANGE` and the file ID; the dummy backend
uses file ID, nanosecond timestamps, and size. Validation happens at
open, not on every read. With fsspec's default `check_files=False`, persistent
entries are reused without source validation until `expiry_time`, so that mode
is appropriate only when external clients use immutable or versioned paths.
Mutations made through the same nfs4fs target invalidate registered persistent
block-cache generations automatically, including renamed or recursively
deleted subtrees. Already-open handles retain their per-handle cached blocks.
`cache_check` controls how often local cache metadata is reloaded and does not
validate the server. `invalidate_cache()` controls directory listings; use
`pop_from_cache()` to evict persistent file data explicitly.

Files opened together with `fsspec.open_files()` share a coordinator. A cache
miss reads the same aligned range from a bounded group of sibling descriptors
through one VFSI vector operation. A requested whole-file read keeps its fast
path for the default read-ahead cache; `cache_type="blockcache"` fills its
bounded LRU block-by-block. Speculative siblings are capped to one block (which
may contain a whole small file). Set `vectorized_buffering=False` to disable
this adaptive fan-out. The persistent `blockcache` wrapper participates in the
same coordinator: it validates source identities in a metadata batch, serves
complete hits locally, and fills sparse misses with vector reads.

Writes remain write-through by default. Set `write_buffering=True` to delay
small writes until `flush()` or `close()`; an `open_files()` write group drains
one block from each file per vector operation. Explicit `flush()` always sends
that file's staged data to the server. Update modes containing `+` retain the
unbuffered seekable implementation, and fsspec transactions retain their
existing disk-spooled commit behavior.

## Directory listing cache

Directory listing caching is disabled by default so that NFS namespaces
modified by other clients remain immediately visible. Enable a
bounded-staleness cache with the standard fsspec options:

```python
fs = fsspec.filesystem(
    "nfs4",
    host="nfs.example",
    root="exports/project",
    use_listings_cache=True,
    listings_expiry_time=1.0,
    max_paths=1024,
)
```

`listings_expiry_time` is the TTL in seconds and `max_paths` bounds the number
of directory paths tracked by fsspec's `DirCache`; it does not bound the number
of entries inside one directory. `ls(..., refresh=True)` bypasses the cached
listing. Native vectorized walks populate the cache for the complete subtree,
so repeated `walk`, `find`, `glob`, and `du` operations can reuse it without
turning a miss into one request per directory.

nfs4fs invalidates affected listings after its own writes and namespace
mutations, including uncertain failures. Changes made by another client become
visible when the TTL expires or when the caller requests a refresh. Use
`use_listings_cache=False` when any period of staleness is unacceptable.

## Production notes

- One filesystem instance owns one native session protected by a mutex.
  Operations on that instance are serialized. Use separate instances with
  `skip_instance_cache=True` when independent connections are required.
- Native calls release the CPython interpreter lock while waiting for storage.
  The extension also declares free-threaded CPython support; each filesystem's
  native-session mutex still serializes that instance.
- On a transport failure, safe idempotent path reads reconnect and retry once
  by default. Mutations are never replayed automatically because the server may
  already have completed an ambiguously failed request. Set
  `auto_reconnect=False` to disable automatic read recovery.
- A process fork is detected before the next operation and creates a fresh
  native session in the child without sending protocol teardown over the
  parent's inherited connection. Do not fork while an operation is in flight
  or share an already-open `Nfs4File` across a fork. Once application or RPC
  helper threads exist, use multiprocessing's `spawn` method. Prefer creating
  filesystem instances after worker processes start.
- Call `close()` or use `with fsspec.filesystem(...) as fs:` to release the
  native session deterministically. Closing also removes the instance from
  fsspec's instance cache.
- `exists`, `isfile`, and `isdir` return `False` for missing paths but propagate
  authentication and connectivity failures. This prevents outages from being
  mistaken for absent data.
- `auto_mkdir` is `False` by default. Enable it only when write operations are
  allowed to create missing parents.
- `root` rejects `.` and `..` components. Use it to keep all paths under an
  export-relative prefix; it is not a substitute for server-side
  authorization.
- Vector batches are bounded by both `batch_size` (128 items) and
  `max_batch_bytes` (64 MiB). Files larger than that byte threshold stream in
  `transfer_chunk_size` chunks (8 MiB). Tune these per server and workload;
  every value must be positive.
- One native session preserves maximum batching by default. For thread-pool
  workloads, set `connection_pool_size` above one; independent path operations
  are distributed across sessions while every open descriptor remains pinned
  to its owning session. Each additional session consumes one server client
  identity and connection, so size the pool deliberately.
- APIs that materialize complete remote results have independent safety
  limits. `read_all_max_total_bytes` bounds every bytes-returning read,
  including whole-file reads, buffered `read(-1)`, `cat`, and `cat_ranges` (16
  MiB); `directory_max_entries` (100,000), `directory_max_path_bytes` (16
  MiB), and `walk_max_depth` (128) bound listings and recursive walks. These
  limits are configurable per filesystem instance and survive reconnects.
- Transaction writes use a disk-backed spool after
  `transaction_spool_threshold` (8 MiB), upload to unpredictable temporary
  names in each destination directory, then expose the batch with a vectorized
  rename. This prevents partial file contents from becoming visible. Like
  fsspec transactions generally, a multi-file commit is staged rather than a
  server-wide atomic transaction: a server failure during the final rename
  batch can expose a prefix of the batch.
- `pipe_file(..., mode="create")` and create-mode `put` use an exclusive server
  create, so concurrent creators cannot silently overwrite one another.

## Local development

On Ubuntu/Debian:

```sh
sudo apt-get install clang libclang-dev pkg-config libntirpc-dev \
  libkrb5-dev libgssglue-dev liburcu-dev
python -m venv .venv
.venv/bin/pip install hypothesis maturin fsspec pytest
cd adapters/nfs4fs
../../.venv/bin/maturin develop
../../.venv/bin/python -m pytest tests
```

Tests use the local dummy backend unless NFS integration variables are
provided. CI runs the same suite against NFSv4.1, NFSv4.2, and patched
server-side COPY. Samba coverage lives with the `vsmbfs` adapter.

The default suite includes a deterministic Hypothesis state machine that runs
randomized filesystem histories against both nfs4fs's dummy backend and
fsspec's `LocalFileSystem` oracle. It compares public results, exception
classes, directory trees, and file contents after every step. Increase its
budget for a longer local fuzz run:

```sh
NFS4FS_FUZZ_EXAMPLES=1000 NFS4FS_FUZZ_STEPS=100 \
  ../../.venv/bin/python -m pytest tests/test_fuzz_parity.py
```
