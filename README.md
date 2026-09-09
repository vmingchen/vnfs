# VFSI: vectorized filesystem interfaces

[![CI](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml/badge.svg)](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/nfs4fs.svg)](https://pypi.org/project/nfs4fs/)
[![Python](https://img.shields.io/pypi/pyversions/nfs4fs.svg)](https://pypi.org/project/nfs4fs/)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

VFSI is a Rust filesystem client built around vector operations. It turns a
batch of independent file operations into a small number of protocol requests,
so applications can benefit from NFSv4 compounds and SMB2/3 related compounds
without mounting a filesystem or rewriting their data path in C.

The repository provides:

- a Rust `VecFs` API with NFSv4.1, NFSv4.2, SMB2/3, and local test backends;
- the [`nfs4fs`](python/) Python package for `fsspec` applications;
- a versioned [C ABI](vfsi-c/) with a checked-in header;
- integration tests against NFS-Ganesha, a patched NFSv4.2 COPY server, and
  Samba dialects from SMB 2.1 through SMB 3.1.1.

## Why vectorize filesystem operations?

Traditional filesystem APIs issue operations one path at a time. On a remote
filesystem, latency can dominate when an application touches many small files.
VFSI accepts arrays of paths, ranges, or rename/copy pairs and lets the backend
combine them into protocol-native compounds. One `fs.cat([...])` or
`fs.pipe({...})` can therefore complete in a bounded number of network round
trips instead of scaling linearly with the file count.

## Python quick start

`nfs4fs` is currently Linux-only and supports CPython 3.9 and newer.

```sh
python -m pip install nfs4fs
```

The installed package registers itself with `fsspec`; no side-effect import is
required:

```python
import fsspec

fs = fsspec.filesystem("nfs4", host="nfs.example", root="exports/data")
fs.pipe({"/a.txt": b"hello", "/b.txt": b"world"})
print(fs.cat(["/a.txt", "/b.txt"]))

# The server can also be supplied by the URL.
with fsspec.open("nfs4://nfs.example/exports/data/a.txt", "rb") as file:
    print(file.read())
```

See the [nfs4fs guide](python/) for NFS and SMB configuration, bulk operations,
failure behavior, source-build dependencies, and current limitations.

## Rust

The `vnfs` crate enables the NFS, SMB, dummy, and server-copy features by
default. Downstream users can disable defaults and select only the backends
they need. Connections negotiate NFSv4.2 and fall back to NFSv4.1. Server-side
COPY is capability-aware and falls back to client-side I/O when unavailable.

The SMB backend negotiates SMB 2.0.2 through SMB 3.1.1, observes server credit
and I/O limits, reconnects transport sessions, and uses server-side copy when
the server advertises it.

## Project status

VFSI and `nfs4fs` are beta software. The core behavior is covered by Rust,
Python, upstream `fsspec` contract, C ABI, NFS, and Samba integration tests, but
operators should read the [production notes](python/#production-notes) before
deploying it for critical data. In particular, NFS authentication is currently
AUTH_SYS, the API is synchronous, and Linux is the only packaged platform.

Issues and focused pull requests are welcome. Start with
[CONTRIBUTING.md](CONTRIBUTING.md); security reports follow
[SECURITY.md](SECURITY.md).

## Background

NFSv4 compounds are defined by the NFS standards, but conventional POSIX-style
APIs rarely expose them to applications. VFSI follows the idea explored in the
FAST '17 paper [vNFS: Maximizing NFS Performance with Compounds and Vectorized
I/O](https://www.usenix.org/conference/fast17/technical-sessions/presentation/chen).
The original research client was implemented in C; this repository provides a
Rust implementation and protocol-neutral bindings.

## License

Licensed under either [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.
