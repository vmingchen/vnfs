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

- `vnfs` is the published Rust compatibility facade;
- `vfsi-c` is the versioned C ABI;
- `nfs4fs` is the Python distribution and fsspec adapter;
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
`VecFs` trait remains the compatibility contract; its one-operation helpers
form the initial scalar view. Async and transactional crates should be added
only when their contracts and implementations exist.

## Platform source tree

The canonical repository is organized by architectural responsibility:

```text
crates/
  vnfs/                 published compatibility facade
  vfsi-core/            shared operations, types, errors, paths, capabilities
  vfsi-sync/            scalar and vectorized synchronous interfaces
  vfsi-nfs/             NFSv4.1 and NFSv4.2 backend
  vfsi-smb/             optional SMB2 and SMB3 backend
  vfsi-local/           local backend and future io_uring implementation
  nfsv41-sys/           generated NFS protocol bindings
bindings/
  c/                    vfsi-c package and checked-in public header
adapters/
  nfs4fs/               PyO3 extension, Python package, and fsspec adapter
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
vfsi-core
    -> vfsi-sync
        -> vfsi-nfs / vfsi-smb / vfsi-local
            -> vnfs facade
                -> vfsi-c / nfs4fs
```

Shared crates must not depend on a backend. Backends may implement optimized
vector operations and report optional capabilities. The `vnfs` facade selects
backends with Cargo features and preserves historical imports. SMB remains
optional and is not enabled by default.

io_uring belongs in the local backend because it is an execution mechanism,
not a network filesystem protocol. Cloud object-store support belongs in
backend packages, split by provider only when semantics or release ownership
requires it.

## Repository classes

| Planned repository | Class | Purpose |
| --- | --- | --- |
| `vmingchen/vnfs` | platform | Canonical development and releases |
| `vfsi/vfsi` | mirror | Approved public snapshots of the platform |
| `vfsi/.github` | organization | Project catalog and contribution routing |
| `vfsi/port-git` | application port | Git loose-object traversal using VFSI |
| `vfsi/port-rsync` | application port | rsync sender-side scans using VFSI |
| `vfsi/infra-nfs-ganesha` | infrastructure | Pinned server used by integration tests |

Application-port repository names use the `port-` prefix. Infrastructure
forks use the `infra-` prefix and must not be presented as application ports.
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

`ecosystem/ports.toml` records both the current and planned repository names
during migration. Every entry pins a full revision, upstream base, integration
branch, maturity, required ABI, and compatibility-test entry point. Platform
CI must never build an unpinned external branch.

Fast platform CI validates the registry and public API compatibility. Scheduled
and manually dispatched ecosystem CI reconstructs each registered revision and
builds it against the current `vfsi-c` ABI. ABI changes are not releasable until
the registered ports pass or explicitly move to a new declared ABI.

## Releases and mirror promotion

Rust crates, the C ABI, and Python distributions are built and published only
from the canonical repository. Package tags are namespace-qualified, such as
`vnfs-v0.0.9` and `nfs4fs-v0.3.0`.

Workspace crates are released in dependency order: `vfsi-core`, `vfsi-sync`,
the selected `vfsi-*` backends, `vnfs`, and finally `vfsi-c` and `nfs4fs`.
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
