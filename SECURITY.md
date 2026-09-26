# Security policy

## Supported versions

Until VFSI reaches 1.0, security fixes are made on the latest `main` branch and
included in the next release. Older prereleases are not maintained separately.

## Recursive removal

The path-taking removal APIs (`rm`, `rm_contents`, `ensure_empty_dir`) are
subject to the entry-point TOCTOU described by
[RUSTSEC-2023-0018](https://rustsec.org/advisories/RUSTSEC-2023-0018.html): a
concurrent actor can replace a path component with a symlink between the caller
naming the path and removal starting. Inside the tree, removal is
handle-relative and safe. Privileged or attacker-influenced callers should root
the removal at an already-open directory with `open_dir` +
`remove_dir_contents_handle` (see the `vnfs` README, "Recursive removal and
path-entry races").

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
