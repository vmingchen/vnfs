# Migrating applications to per-operation results

These are unreleased source adaptations for the vector-results migration.
The patches apply to the currently checked-out HEADs of the four-port source
set; they do not change those repositories or claim published compatibility.
Patch bases:
coreutils 544d1ed1f919faad23b5b1e4c860328cc4a31264;
findutils dc2809ebe017251f9e3217fc6f4141a687ebddd8.

Only the Rust ports need source/dependency patches. Git and rsync continue
through the existing C ABI v3 and optional-symbol loader.

## Rust callers

Every canonical vector method now returns
[VecResult](../../crates/vfsi-core/src/outcome.rs):
an outer error rejects the whole submission; an accepted submission returns
one OpResult per input, in input order. Completion order remains independent.
A successful sibling remains usable when another input fails.

Use VfsiExt all-success conveniences such as getattrs_all, listdirs_all,
and removes_all where the application intentionally requires every result.
These wrappers select an error in input order and lose successful sibling
values. They do not imply a completed prefix, rollback, or safe replay.
Keep VecResult directly whenever the application needs selective fallback,
partial results, or resource ownership.

In particular, the coreutils COPY path examines each outcome. Only
FailureState::NotAttempted permits ordinary fallback for that input.
Failed and Unknown are surfaced even when the cause says Unsupported;
their private staging files are cleaned without publishing or replaying.
Known nonprefix successes are also cleaned if the aggregate cp operation fails.
An invalid result count is a hard failure and cleans staging paths.
Four regression tests cover these policies.

## Reviewable port patches

- [coreutils-vecresult.patch](coreutils-vecresult.patch): shared metadata,
  directory/removal collection, du visitor collection, safe cp COPY handling,
  and the registry dependency pin.
- [findutils-vecresult.patch](findutils-vecresult.patch): dependency pin only;
  its existing scalar conveniences already hide aggregate collection.

Apply from the corresponding port repository root with git apply after checking
the base revision. Resolve a newer upstream base deliberately rather than
blindly applying these patches. For local verification, the dependency
is overridden only in the Cargo invocation:

    cargo test --offline --config 'patch.crates-io.vnfs.path="/home/ubuntu/vnfs02/crates/vnfs"' --features vnfs

The patch manifests themselves use vnfs = "=0.0.32", without a local path.
Do not check a lockfile containing local source resolution into a release port;
regenerate it from the registry after publication.

## Coordinated release order

Prepared versions are vfsi-core/sync/local/posix/uring/nfs/smb 0.2.0,
vnfs 0.0.32, vfsi-c 0.3.16, nfs4fs 0.3.13, vfsi-fsspec 0.1.13,
and vsmb/vsmbfs 0.1.7. The shared private native adapter is 0.1.7.
nfsv41-sys stays at 0.1.15. These values are local preparation, not publication.

1. Review and commit the complete buildable Rust migration. Publish dependencies
   in dependency order (core, sync, local, posix/uring/nfs/smb, vnfs), verifying
   each archive against the newly available registry cohort.
2. Apply and review the Rust port patches. After vnfs is available, regenerate
   their registry lockfiles, run port tests, then create actual port commits.
3. Update ecosystem provenance only to real reviewed port commits and published
   versions. Keep existing provenance intact until replacements exist.
4. Publish the C/Python updates as needed, preserving C ABI v3 and existing
   Python return/exception conventions. Test both present and absent optional
   C symbols; a version bump is not permission to change ABI layouts.
5. Require same-commit CI and all four port compatibility jobs before mirror
   promotion. Disposable local-source tests are useful prepublication evidence,
   not substitutes for that release gate.

No commit, push, publication, remote CI success, or mirror promotion is implied
by these prepared patches. Those steps remain outside the uncommitted migration.
