# Security policy

## Supported versions

Until VFSI reaches 1.0, security fixes are made on the latest `main` branch and
included in the next release. Older prereleases are not maintained separately.

## Recursive removal

The path-taking removal APIs (`remove_dir_all`, `remove_dir_contents`, and
`vremove`) are
subject to the entry-point TOCTOU described by
[RUSTSEC-2023-0018](https://rustsec.org/advisories/RUSTSEC-2023-0018.html): a
concurrent actor can replace a path component with a symlink between the caller
naming the path and removal starting. A no-follow preflight check does not
eliminate that race. Only remove directories the application intentionally owns,
using trusted ancestor paths; path-based removal is not a security sandbox or a
transaction. Names are resolved for each call and do not retain object identity
across renames or path replacement.

The NFS path remover retains directory filehandles internally while traversing
the tree. This does not pin the initial pathname between separate application
calls, and other backends' traversal guarantees may differ. Explicit
retained-directory workflows use `Vfsi::vopen_dirs` and `vremove_dir_contents`
on backends with genuine handles; even opening such a handle does not secure
attacker-controlled ancestors. See
[recursive removal and races](crates/vnfs/src/guides/failure_recovery.md#recursive-removal-and-races).

## Reporting a vulnerability

Please use a [private GitHub security
advisory](https://github.com/vmingchen/vnfs/security/advisories/new). Do not
open a public issue for suspected credential exposure, authentication bypass,
path confinement failure, memory-safety issue, or remotely triggerable denial
of service.

Include affected versions, backend/server details, impact, reproduction steps,
and any proposed mitigation. Remove live credentials and sensitive production
data. You should receive an acknowledgement within seven days; disclosure and
release timing will be coordinated through the advisory.
