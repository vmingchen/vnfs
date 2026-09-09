# nfs4fs

`nfs4fs` is a Linux `fsspec` implementation backed by VFSI's vectorized NFSv4
and SMB2/3 clients. Bulk `fsspec` operations are translated into protocol-native
compounds, reducing round trips for workloads with many small files.

## Install

```sh
python -m pip install nfs4fs
```

Published wheels use CPython's stable ABI and support standard CPython 3.9 and
newer on Linux x86-64 and AArch64. A separate CPython 3.14 free-threaded wheel
is built and tested on x86-64. A source build additionally requires Rust, CMake,
Clang, Git, Kerberos/GSS development headers, and userspace-RCU development
headers. The extension statically links libntirpc and bundles non-platform
shared-library dependencies into release wheels.

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

fs.pipe({"/part-0": b"hello", "/part-1": b"world"})
parts = fs.cat(["/part-0", "/part-1"])

with fsspec.open("nfs4://nfs.example/exports/project/part-0", "rb") as file:
    print(file.read())
```

The client negotiates NFSv4.2 and falls back to NFSv4.1. Set
`minor_version=1` or `minor_version=2` to require a specific version. The
current NFS transport uses TCP and AUTH_SYS. Kerberos flavors are not yet
exposed by the Python package.

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

## SMB2/3

Use the protocol-neutral `vfsi` name and select the SMB backend explicitly:

```python
fs = fsspec.filesystem(
    "vfsi",
    backend="smb",
    host="samba.example",
    share="data",
    username="alice",
    password="secret",
    domain="WORKGROUP",
    root="team-a",
)
fs.pipe_file("/hello.txt", b"hello over SMB")
print(hex(fs.smb_dialect()))
```

Empty credentials request guest access. SMB paths must be valid UTF-8. The SMB
backend does not advertise Unix link or ownership semantics; capability-aware
operations use following metadata where possible.

Do not embed credentials in URLs. Supply them as storage options or through
the normal `fsspec` configuration mechanisms, and avoid logging serialized
filesystem objects containing secrets.

## Operations that vectorize

The largest benefit comes from giving `fsspec` multiple paths at once:

- `cat`, `cat_ranges`, and `open_files` batch reads;
- `pipe` and `put` batch writes;
- recursive `get`, `put`, `cp`, `rm`, and tree walks use vector metadata and
  mutation operations;
- `cp_file`/`copy` use NFSv4.2 or SMB server-side copy when available, with a
  client-side fallback.

`minor_version()`, `capabilities()`, `server_copy_enabled()`, `smb_dialect()`,
`compound_stats()`, and `rpc_stats()` are available on the native client for
feature inspection and diagnostics.

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
  export- or share-relative prefix; it is not a substitute for server-side
  authorization.
- Vector batches are bounded by both `batch_size` (128 items) and
  `max_batch_bytes` (64 MiB). Files larger than that byte threshold stream in
  `transfer_chunk_size` chunks (8 MiB). Tune these per server and workload;
  every value must be positive.
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
sudo apt-get install clang libclang-dev cmake pkg-config \
  libkrb5-dev libgssglue-dev liburcu-dev
python -m venv .venv
.venv/bin/pip install maturin fsspec pytest
cd python
../.venv/bin/maturin develop
../.venv/bin/python -m pytest tests
```

Tests use the local dummy backend unless NFS/SMB integration variables are
provided. CI runs the same suite against NFSv4.1, NFSv4.2, patched server-side
COPY, SMB 2.1 guest access, and authenticated SMB 3.1.1.
