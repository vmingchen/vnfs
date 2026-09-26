# VFSI: vectorized filesystem interfaces

[![CI](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml/badge.svg)](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/vnfs.svg)](https://crates.io/crates/vnfs)
[![PyPI](https://img.shields.io/pypi/v/nfs4fs.svg)](https://pypi.org/project/nfs4fs/)
[![Python](https://img.shields.io/pypi/pyversions/nfs4fs.svg)](https://pypi.org/project/nfs4fs/)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

VFSI is a Rust filesystem client built around vector operations. It turns a
batch of independent file operations into a small number of protocol requests,
so applications can benefit from NFSv4 compounds and SMB2/3 related compounds
without mounting a filesystem or rewriting their data path in C.

The repository provides:

- composable scalar (`sfsi`) and vectorized (`vfsi`) Rust API facets;
- NFSv4.1, NFSv4.2, optional SMB2/3, and local backends;
- the [`nfs4fs`](adapters/nfs4fs/) NFS fsspec package, the low-level Python
  [`vsmb`](adapters/vsmb/) client, and the [`vsmbfs`](adapters/vsmbfs/) fsspec
  adapter for SMB;
- a versioned [C ABI](bindings/c/) with a checked-in header;
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

For SMB, install `vsmb` for the low-level vector API or `vsmbfs` for fsspec:

```python
import fsspec

fs = fsspec.filesystem("vsmbfs", host="samba.example", share="data")
print(fs.ls("/"))
```

See the [nfs4fs guide](adapters/nfs4fs/) and [vsmbfs guide](adapters/vsmbfs/)
for protocol-specific configuration.

## Project organization

The platform is organized as a modular monorepo: shared contracts, synchronous
interfaces, protocol backends, bindings, and adapters are separate workspace
packages while the published `vnfs` crate preserves the NFS-facing API.
Modified
third-party applications live in independent VFSI organization repositories
and are registered at pinned revisions rather than vendored or added as
submodules.

See [VFSI project organization](docs/PROJECT_ORGANIZATION.md) for the source
tree, repository ownership, API facets, port naming, maturity, compatibility,
and mirror-promotion policies.

## Rust

Use `vnfs` for the NFS-focused compatibility crate, `vfsi-core` and
`vfsi-sync` for backend-neutral interfaces, and the protocol crates
`vfsi-nfs`, `vfsi-smb`, and `vfsi-local` when selecting backends directly.
This keeps protocol dependencies out of packages that do not use them.

NFS connections negotiate NFSv4.2 and fall back to NFSv4.1. Rust clients use
AUTH_SYS by default and can opt into Kerberos RPCSEC_GSS (`krb5` or `krb5i`)
with the `rpcsec-gss` feature. Server-side COPY is capability-aware
and falls back to client-side I/O when unavailable. The
separate `vfsi-smb` backend negotiates SMB 2.0.2 through SMB 3.1.1, observes
server credit and I/O limits, reconnects transport sessions, and uses
server-side copy when the server advertises it.

## Project status

VFSI and its Python packages are beta software. The core behavior is covered
by Rust, Python, upstream `fsspec` contract, C ABI, NFS, and Samba integration
tests. AUTH_SYS remains the default; Rust and Python clients can opt into
Kerberos RPCSEC_GSS (`krb5` or `krb5i`). The private-realm integration test
covers authentication, renewable-ticket expiry, and reconnect.
The API is synchronous, and Linux is the only native-package platform. Read-only NFS
operations recover automatically from transport and session failures with
bounded reconnect/reopen attempts; mutations are not replayed when a lost
reply makes their outcome ambiguous. See the [`vnfs` production notes and
failure semantics](crates/vnfs/README.md#failure-and-recovery-semantics).

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
