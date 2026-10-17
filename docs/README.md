# VFSI documentation

Start with the [Rust crate README](../crates/vnfs/README.md), its
[canonical examples](../crates/vnfs/examples), or the
[nfs4fs README](../adapters/nfs4fs/README.md) to try the libraries.

## Current guides

| Guide | Purpose |
|---|---|
| [Rust API architecture](RUST_API.md) | `Vfsi`/`VfsiExt`, object-reference mappings, ownership, errors, recovery, and resource budgets. |
| [Backend implementation](BACKENDS.md) | Native execution traits, adapter responsibilities, fault-testing requirements, and remaining compatibility aliases. |
| [Project organization](PROJECT_ORGANIZATION.md) | Package/repository ownership, platform boundaries, port registration, and publication policy. |

For local test selection and port checks, use [scripts/README.md](../scripts/README.md).
NFS authentication, failure recovery, and standard-I/O examples are maintained
with the [Rust crate guides](../crates/vnfs/src/guides) and compiled into rustdoc.

## Performance and historical records

| Record | Scope |
|---|---|
| [Rust performance notes and audits](RUST_PERFORMANCE.md) | Current implementation notes, dated allocation/scheduler audits, and follow-up candidates. |
| [io_uring versus POSIX](uring-benchmark.md) | October 10–11, 2026 experiments; distinguish borrowed/owned reads and ring/adaptive execution. |
| [Complete-write scheduler benchmark](write-scheduler-benchmark.md) | October 11, 2026 paired comparisons, checked-in raw samples, and build-verification evidence. |
| [Release build times](RELEASE_BUILD_TIMES.md) | Append-only observed release costs, with source revisions and measurement-method caveats. |

Historical test counts and timings describe their recorded revisions, not the
current checkout. Preserve observations when updating commands; do not relabel
old measurements as new results. Build-cache and `/tmp` paths are not durable
evidence. Link retained records or explicitly state when artifacts are unavailable.
