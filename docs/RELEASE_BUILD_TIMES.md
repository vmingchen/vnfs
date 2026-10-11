# Release build-time baseline

These are observed GitHub Actions timings, retained for a separate release-speed
investigation. Durations are wall-clock job or step times from the GitHub Actions
API, not estimates of CPU time. Jobs run concurrently.

## 2026-10-10 Python publications

All four releases used `50ec71ee12c95d36a2b401f59041af28dfd83405`.
Workflow wall time includes runner queueing; job durations exclude runner wait
time. Runs occurred on October 11 UTC (October 10 in America/New_York).

| Release run | Wall time | Result |
| --- | ---: | --- |
| [vsmb 0.1.3](https://github.com/vmingchen/vnfs/actions/runs/38106861340) | 2m 50s | Success |
| [vfsi-fsspec 0.1.6](https://github.com/vmingchen/vnfs/actions/runs/38107073398) | 1m 16s | Success |
| [vsmbfs 0.1.3](https://github.com/vmingchen/vnfs/actions/runs/38107165724) | 1m 55s | Success |
| [nfs4fs 0.3.6](https://github.com/vmingchen/vnfs/actions/runs/38107165564) | 22m 48s | Success |

| Release | Job | Duration |
| --- | --- | ---: |
| vsmb 0.1.3 | Verify CI | 5s |
| vsmb 0.1.3 | Source distribution | 21s |
| vsmb 0.1.3 | SBOM | 1m 17s |
| vsmb 0.1.3 | x86_64 abi3 wheel | 1m 20s |
| vsmb 0.1.3 | x86_64 CPython 3.14 free-threaded wheel | 1m 20s |
| vsmb 0.1.3 | aarch64 abi3 wheel | 2m 10s |
| vsmb 0.1.3 | Publish | 30s |
| vfsi-fsspec 0.1.6 | Verify CI | 4s |
| vfsi-fsspec 0.1.6 | Build and test | 34s |
| vfsi-fsspec 0.1.6 | Publish | 37s |
| vsmbfs 0.1.3 | Verify CI | 7s |
| vsmbfs 0.1.3 | Build and test | 34s |
| vsmbfs 0.1.3 | Publish | 27s |
| nfs4fs 0.3.6 | Verify CI | 5s |
| nfs4fs 0.3.6 | Source distribution | 20s |
| nfs4fs 0.3.6 | SBOM | 1m 15s |
| nfs4fs 0.3.6 | x86_64 abi3 wheel | 2m 32s |
| nfs4fs 0.3.6 | x86_64 CPython 3.14 free-threaded wheel | 2m 33s |
| nfs4fs 0.3.6 | aarch64 abi3 wheel under QEMU | 21m 37s |
| nfs4fs 0.3.6 | Installed wheel against live NFS RPCSEC_GSS | 28s |
| nfs4fs 0.3.6 | Publish | 34s |

The vsmb wheel build steps took 1m 11s on each x86_64 variant and 1m 16s
on aarch64; the corresponding installed-wheel checks took 4s, 4s, and 42s.
For the pure-Python packages, distribution builds took 11s (vfsi-fsspec) and
10s (vsmbfs); installed-wheel checks took 5s and 6s respectively.

The nfs4fs portable-wheel build steps took 2m 21s (x86_64 abi3), 2m 22s
(x86_64 free-threaded), and **21m 01s** (aarch64 under QEMU). Their repaired-wheel
checks took 3s, 4s, and 26s respectively. The emulated aarch64 build dominated
the release's critical path.

The source [CI run](https://github.com/vmingchen/vnfs/actions/runs/38106414359)
passed on attempt 2. Its initial NFSv4.1 latency benchmark failed with
`NFS4ERR_STALE_CLIENTID` (10022). Only the failed job was rerun, on unchanged
source, and passed; no check was weakened. This is an observed intermittent
failure, not a demonstrated fix. All four release-tag CI runs passed.

After publication, all four Python packages were installed from PyPI into a
fresh VM virtualenv and tested outside the checkout: 131 tests passed with live
NFSv4.2 (including required server COPY), 104 with live NFSv4.1, and 21 for
SMB/shared-engine package interoperability. The externally coordinated server
restart test was excluded from both local NFS runs; no selected tests skipped.
Dependency validation also passed.

These are observed timings, not a controlled performance comparison. No build
strategy was changed.

## 2026-10-10 Rust publications

All crates were published from the clean checkout of
`a5fd95fa1c6bb8f72bf8ee267224902b55413489`, after its
[18-job CI run](https://github.com/vmingchen/vnfs/actions/runs/38104685358)
succeeded. These measurements used the VM's existing build cache, not a cold
build. Publications ran sequentially in dependency order.

Verification times are Cargo's reported `dev` build durations. Total times
are measured wall time for packaging, verification, upload, and registry
indexing wait; they must not be interpreted as compilation time alone.

| Crate | Verification build | Total publish command |
| --- | ---: | ---: |
| vfsi-core 0.1.9 | 1.01s | 2.09s |
| vfsi-sync 0.1.10 | 2.34s | 3.18s |
| vfsi-local 0.1.8 | 3.76s | 64.16s |
| vfsi-posix 0.1.0 | 1.64s | 3.37s |
| vfsi-uring 0.1.0 | 1.32s | 3.12s |
| vfsi-nfs 0.1.9 | 5.50s | 7.90s |
| vfsi-smb 0.1.7 | 4.77s | 6.68s |
| vnfs 0.0.21 | 5.06s | 7.39s |

The vfsi-local upload succeeded but its indexing wait timed out. The next
crate successfully downloaded that version from crates.io and verified
against it; no upload was repeated. Runs occurred on October 11 UTC
(October 10 in America/New_York).

## 2026-10-04 coordinated releases

Workflow wall time includes runner queueing; job durations exclude runner wait
time. These runs occurred after midnight UTC on October 5 (October 4 in
America/New_York).

| Release run | Wall time | Result |
| --- | ---: | --- |
| [vfsi-fsspec 0.1.5](https://github.com/vmingchen/vnfs/actions/runs/37253299887) | 2m 30s | Success |
| [nfs4fs 0.3.5](https://github.com/vmingchen/vnfs/actions/runs/37254374106) | 23m 04s | Success |
| [vsmb 0.1.2](https://github.com/vmingchen/vnfs/actions/runs/37253300562) | 4m 46s | Success |
| [vsmbfs 0.1.2](https://github.com/vmingchen/vnfs/actions/runs/37253714637) | 1m 11s | Success |

| Release | Job | Duration |
| --- | --- | ---: |
| vfsi-fsspec 0.1.5 | Verify CI | 7s |
| vfsi-fsspec 0.1.5 | Build and test | 40s |
| vfsi-fsspec 0.1.5 | Publish | 29s |
| nfs4fs 0.3.5 | Verify CI | 5s |
| nfs4fs 0.3.5 | Source distribution | 18s |
| nfs4fs 0.3.5 | SBOM | 1m 34s |
| nfs4fs 0.3.5 | x86_64 abi3 wheel | 2m 36s |
| nfs4fs 0.3.5 | x86_64 CPython 3.14 free-threaded wheel | 2m 34s |
| nfs4fs 0.3.5 | aarch64 abi3 wheel under QEMU | 21m 37s |
| nfs4fs 0.3.5 | Installed wheel against live NFS RPCSEC_GSS | 32s |
| nfs4fs 0.3.5 | Publish | 24s |
| vsmb 0.1.2 | Verify CI | 5s |
| vsmb 0.1.2 | Source distribution | 29s |
| vsmb 0.1.2 | SBOM | 1m 18s |
| vsmb 0.1.2 | x86_64 abi3 wheel | 1m 19s |
| vsmb 0.1.2 | x86_64 CPython 3.14 free-threaded wheel | 1m 25s |
| vsmb 0.1.2 | aarch64 abi3 wheel | 2m 48s |
| vsmb 0.1.2 | Publish | 27s |
| vsmbfs 0.1.2 | Verify CI | 7s |
| vsmbfs 0.1.2 | Build and test | 39s |
| vsmbfs 0.1.2 | Publish | 23s |

The initial [nfs4fs 0.3.5 attempt](https://github.com/vmingchen/vnfs/actions/runs/37253539775)
was canceled after the x86_64 wheel checks rejected an absent but unused RCU
library. The wheels installed and imported successfully, but nothing was
published from that attempt. The packaging checker was corrected to require
the exact RCU library only when an ELF dependency actually needs it, with
regressions for absent dependencies, wrong RCU flavors, and dynamic ntirpc.
The unpublished nfs4fs tag was advanced after CI passed. Published release
tags were unchanged. Failed/canceled attempts are not success-time baselines.

The successful nfs4fs aarch64 **Build the portable wheel** step took
**21m 00s**, dominating the release's critical path. This is observed timing,
not a controlled performance comparison; no build strategy was changed.

## 2026-10-02 coordinated releases

Workflow wall time includes runner queueing, from workflow creation to final
update. Job durations exclude time waiting for a runner. The runs occurred
after midnight UTC on October 3 (October 2 in America/New_York).

| Release run | Wall time | Result |
| --- | ---: | --- |
| [vfsi-fsspec 0.1.4](https://github.com/vmingchen/vnfs/actions/runs/37090645093) | 2m 24s | Success |
| [nfs4fs 0.3.4](https://github.com/vmingchen/vnfs/actions/runs/37090828786) | 21m 14s | Success |
| [vsmb 0.1.1](https://github.com/vmingchen/vnfs/actions/runs/37090645062) | 3m 15s | Success |
| [vsmbfs 0.1.1](https://github.com/vmingchen/vnfs/actions/runs/37090920901) | 1m 16s | Success |

| Release | Job | Duration |
| --- | --- | ---: |
| vfsi-fsspec 0.1.4 | Verify CI | 7s |
| vfsi-fsspec 0.1.4 | Build and test | 40s |
| vfsi-fsspec 0.1.4 | Publish | 26s |
| nfs4fs 0.3.4 | Verify CI | 5s |
| nfs4fs 0.3.4 | Source distribution | 16s |
| nfs4fs 0.3.4 | SBOM | 1m 25s |
| nfs4fs 0.3.4 | x86_64 abi3 wheel | 2m 18s |
| nfs4fs 0.3.4 | x86_64 CPython 3.14 free-threaded wheel | 2m 19s |
| nfs4fs 0.3.4 | aarch64 abi3 wheel under QEMU | 20m 00s |
| nfs4fs 0.3.4 | Installed wheel against live NFS RPCSEC_GSS | 26s |
| nfs4fs 0.3.4 | Publish | 38s |
| vsmb 0.1.1 | Verify CI | 4s |
| vsmb 0.1.1 | Source distribution | 21s |
| vsmb 0.1.1 | SBOM | 1m 18s |
| vsmb 0.1.1 | x86_64 abi3 wheel | 1m 23s |
| vsmb 0.1.1 | x86_64 CPython 3.14 free-threaded wheel | 1m 28s |
| vsmb 0.1.1 | aarch64 abi3 wheel | 2m 38s |
| vsmb 0.1.1 | Publish | 27s |
| vsmbfs 0.1.1 | Verify CI | 6s |
| vsmbfs 0.1.1 | Build and test | 41s |
| vsmbfs 0.1.1 | Publish | 22s |

This records observed timing only; release build optimizations remain a
separate effort. Cached and uncached build durations are not interchangeable.
The nfs4fs aarch64 **Build the portable wheel** step took **19m 24s**,
dominating its critical path. No build strategy was changed in this release.

## 2026-09-26 releases

| Release run | Wall time | Result |
| --- | ---: | --- |
| [vfsi-fsspec 0.1.3](https://github.com/vmingchen/vnfs/actions/runs/36272092777) | 1m 10s | Success |
| [nfs4fs 0.3.3](https://github.com/vmingchen/vnfs/actions/runs/36272208456) | 21m 23s | Success |

### vfsi-fsspec 0.1.3 jobs

| Job | Duration |
| --- | ---: |
| Build and test | 35s |
| Verify CI | 11s |
| Publish | 29s |

### nfs4fs 0.3.3 jobs

| Job | Duration |
| --- | ---: |
| Verify CI | 5s |
| Source distribution | 10s |
| SBOM | 1m 29s |
| x86_64 abi3 wheel | 2m 41s |
| x86_64 CPython 3.14 free-threaded wheel | 2m 32s |
| aarch64 abi3 wheel under QEMU | **20m 16s** |
| Installed wheel against live NFS RPCSEC_GSS | 30s |
| Publish | 29s |

The aarch64 wheel's **Build the portable wheel** step took **19m 35s**. Its
manylinux wheel smoke test took 26s. The live-NFS job spent 15s installing the
private server and 7s exercising the published dependency with the repaired
wheel. Thus the emulated aarch64 build dominated the critical path; this record
does not change the build strategy.

For comparison, the [nfs4fs 0.3.2 release](https://github.com/vmingchen/vnfs/actions/runs/35478647035)
had a successful aarch64 wheel job lasting 12m 38s. This is one prior sample,
not evidence of a stable performance difference.
