# VFSI project organization

VFSI is the umbrella project for filesystem interfaces that let applications
express work as scalar operations, vectors of independent operations,
asynchronous execution, and transactions. It grew from the vNFS research
described in the FAST '17 paper, but the project is no longer limited to NFS
or to one implementation language.

This document defines the boundaries between the platform, protocol backends,
language adapters, application ports, and test infrastructure. New code and
repositories should follow these boundaries instead of creating a repository
for every combination of API, protocol, and language.

## Project identity and source of truth

The project uses **VFSI** as its public umbrella name. Existing package names
remain stable:

- `vnfs` is the published NFS-focused Rust compatibility facade;
- `vfsi-smb` is the standalone Rust SMB backend;
- `vfsi-c` is the versioned C ABI;
- `nfs4fs` is the NFS Python distribution and fsspec adapter;
- `vsmb` is the low-level vectorized SMB Python distribution;
- `vsmbfs` is the fsspec adapter layered on `vsmb`;
- `nfsv41-sys` is the low-level NFS protocol binding package.

`https://github.com/vmingchen/vnfs` is the canonical platform repository. It
hosts development, issues, pull requests, CI, and package releases. A future
`https://github.com/vfsi/vfsi` repository is a manually promoted public mirror,
not a second writable source of truth. Mirror promotion must be explicit,
CI-gated, and fast-forward-only.

Application ports and infrastructure forks are independently maintained under
the `vfsi` organization because they retain their upstream projects' histories,
licenses, and contribution conventions. The platform repository registers
tested revisions but does not vendor them or include them as submodules.

VFSI is a personal academic project derived from the maintainer's research and
is independent of the maintainer's employer. Contributions must not contain
employer confidential information, proprietary data, credentials, or code the
contributor is not authorized to publish.

## Interface facets

The interface names describe composable facets, not separate products:

| Facet | Responsibility |
| --- | --- |
| `sfsi` | Singular/scalar filesystem operations |
| `vfsi` | Vector operations that expose batching and protocol compounds |
| `afsi` | Asynchronous execution of scalar or vector operations |
| `tfsi` | Transaction planning, validation, commit, and failure reporting |

Cardinality, execution, and atomicity are different dimensions. Async and
transactional work must reuse the same operation and result types as the sync
interfaces instead of duplicating a complete filesystem API. The current
`NativeFileSystem` is the aggregate Rust-native scalar contract, composed
from focused descriptor, metadata, directory, namespace, link, and copy
traits; `VectorFileSystem` adds native batching. `VecFs` remains the backend/C compatibility contract during
migration. See [RUST_API.md](RUST_API.md) for ownership, typed requests,
failure outcomes, protocol-extension, and compatibility boundaries. Async and
transactional crates should be added only when their contracts and
implementations exist.

## Platform source tree

The canonical repository is organized by architectural responsibility:

```text
crates/
  vnfs/                 published NFS-focused compatibility facade
  vfsi-core/            shared operations, types, errors, paths, capabilities
  vfsi-sync/            scalar and vectorized synchronous interfaces
  vfsi-nfs/             NFSv4.1 and NFSv4.2 backend
  vfsi-smb/             optional SMB2 and SMB3 backend
  vfsi-local/           local backend and future io_uring implementation
  nfsv41-sys/           generated NFS protocol bindings
bindings/
  c/                    vfsi-c package and checked-in public header
adapters/
  vfsi-python/          private shared PyO3 binding implementation
  vfsi-fsspec/          backend-neutral fsspec engine
  nfs4fs/               NFS native extension and fsspec facade
  vsmb/                 low-level vectorized SMB Python client
  vsmbfs/               fsspec facade over vsmb
protocols/
  nfsv4/                protocol source material
third-party/
  libntirpc-sys/        pinned, patched build dependency
ecosystem/
  ports.toml            registered and tested external repositories
docs/                   architecture and compatibility policy
ci/                     integration servers and ecosystem tests
```

The dependency direction is deliberately one-way:

```text
vfsi-core -> vfsi-sync -> vfsi-nfs -> vnfs
                       -> vfsi-smb
                       -> vfsi-local

vnfs + vfsi-smb -------------------> vfsi-c
vfsi-python + vfsi-fsspec ---------> nfs4fs / vsmb / vsmbfs
vsmb + vfsi-fsspec ----------------> vsmbfs
```

Shared crates must not depend on a backend. Backends may implement optimized
vector operations and report optional capabilities. The `vnfs` facade is
NFS-focused and preserves historical NFS imports; Rust SMB consumers depend on
`vfsi-smb` directly. Cross-protocol bindings such as `vfsi-c` compose backend
crates directly instead of routing them through `vnfs`.
Shared backend contract assertions live behind `vfsi-sync`'s non-default
`test-support` feature so published backends can reuse them without depending
on an unpublished helper crate.

Every first-party public package uses `vfsi` as a registry discovery keyword.
Protocol-specific keywords remain on their owning packages: for example,
`nfs` belongs on `vnfs`, `vfsi-nfs`, and `nfs4fs`, while `smb` belongs on
`vfsi-smb`, `vsmb`, and `vsmbfs`. Third-party packages keep their upstream
metadata.

io_uring belongs in the local backend because it is an execution mechanism,
not a network filesystem protocol. Cloud object-store support belongs in
backend packages, split by provider only when semantics or release ownership
requires it.

## Repository classes

| Repository | Class | Purpose |
| --- | --- | --- |
| `vmingchen/vnfs` | platform | Canonical development and releases |
| `vfsi/vfsi` | mirror | Approved public snapshots of the platform |
| `vfsi/.github` | organization | Project catalog and contribution routing |
| `vfsi/vfsi-port-git` | application port | Git loose-object traversal using VFSI |
| `vfsi/vfsi-port-rsync` | application port | rsync sender-side scans using VFSI |
| `vfsi/vfsi-infra-nfs-ganesha` | infrastructure | Pinned server used by integration tests |

Application-port repository names use the `vfsi-port-` prefix. Infrastructure
forks use the `vfsi-infra-` prefix and must not be presented as application ports.
Repository descriptions and READMEs must identify the upstream project, the
integration branch, supported VFSI ABI, build instructions, tested revision,
and maturity.

Maturity values are:

- `experimental`: useful for development but without a compatibility promise;
- `validated`: continuously tested against a declared platform/ABI range;
- `maintained`: validated and covered by an explicit release and support policy.

Integration branches in third-party forks may be maintained as rebased patch
stacks. Rewrites must use `--force-with-lease`; validated release tags are
immutable. Port contributions belong in the port repository, not in the
platform mirror.

## Registration and compatibility

`ecosystem/ports.toml` records each canonical repository name. An entry may
temporarily include a planned repository or branch name during an active
migration. Every entry pins a full revision, upstream base, integration branch,
maturity, required ABI, and compatibility-test entry point. Platform CI must
never build an unpinned external branch.

Fast platform CI validates the registry and public API compatibility. Scheduled
and manually dispatched ecosystem CI reconstructs each registered revision and
builds it against the current `vfsi-c` ABI. ABI changes are not releasable until
the registered ports pass or explicitly move to a new declared ABI.

## Releases and mirror promotion

Rust crates, the C ABI, and Python distributions are built and published only
from the canonical repository. Package tags are namespace-qualified, such as
`vnfs-v0.0.10` and `nfs4fs-v0.3.1`.

Workspace crates are released in dependency order: `vfsi-core`, `vfsi-sync`,
the selected `vfsi-*` backends, `vnfs`, and then `vfsi-c`. Python packages are
released in dependency order: `vfsi-fsspec`, `nfs4fs` and `vsmb`, then
`vsmbfs`. The `nfs4fs-python`, `vsmb-python`, and `vfsi-python-native` Cargo
packages are private build crates source-linked into native Python
distributions; they are not published independently on crates.io.
CI packages and tests every public Rust crate from its generated crate archive.
It builds every Python distribution, installs each one in an isolated virtual
environment with only its declared dependency closure, and runs package-level
smoke, typing, and supported-version checks before release workflows may
publish the artifacts.
The facade must never be published with a dependency version that is not
already available from crates.io. The NFS implementation uses an `ffi` feature;
docs.rs disables that feature because its offline builder cannot fetch and
compile the pinned native libntirpc source.

The `Promote VFSI mirror` workflow requires an explicit canonical commit, a
successful CI run for that commit, and approval through the `vfsi-publish`
environment. It pushes only the mirror's `main` branch and explicitly selected
release tags. It refuses divergent history and never performs an unrestricted
mirror or force push.

The mirror directs issues and pull requests to the canonical repository. It is
for organization-level discovery and stable links, not parallel development.

## Adding a component

Before creating a repository, determine whether the component needs independent
ownership, history, licensing, or releases. Otherwise add it as a workspace
package or adapter in the canonical monorepo. Do not create repositories named
after combinations such as `async-nfs-python`; API facets, backends, and
bindings compose inside the platform.

New public APIs require compatibility tests and an architecture decision.
New backends require capability documentation, scalar/vector contract tests,
and live integration coverage. New application ports require registry metadata,
an upstream provenance document, and a reproducible compatibility test.
