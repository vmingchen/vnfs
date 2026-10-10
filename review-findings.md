# Review of committed vnfs02 changes

Reviewed commits: `9385356` and `de95e72` (tip
`de95e72f6d1fb3bb08d058bb9a9f2e6e15027986`). Uncommitted changes were excluded.

## Finding

### [P2] Retry interrupted writes without replaying completed progress

Location: `crates/vfsi-core/src/api/io.rs:155-156` at the reviewed tip.

The new `FileIo::write_all` immediately propagates an `Interrupted` error from
`write_at(buffer, true)`, contrary to standard Rust `Write::write_all` behavior,
which retries interrupted writes. An isolated reproducer used a backend that
returned `EINTR` before writing any bytes on its first call and would succeed
on its second call. The adapter returned `Interrupted` after only one call.

Handle interruption at a boundary where completed progress is known. Do not
blindly retry the entire completion operation: earlier short-write waves may
already have succeeded, and replaying their payload could duplicate append
data. Add regression coverage for interruption before progress and after
acknowledged short-write progress, retaining no-replay behavior for ambiguous
transport failures.

## Validation and disposition

- Server-independent Rust checks: passed, including C bindings and detached
  Python adapter checks.
- Live NFSv4.2: 122 tests passed.
- Live NFSv4.1: 122 tests passed.
- The isolated interrupted-write reproducer returned `Interrupted`; backend
  call count was 1 instead of the expected retry and success.
- No merge was performed because of this finding. Neither worktree's existing
  changes were modified, and nothing was pushed.

## Resolution

Addressed in the uncommitted vnfs02 changes on 2026-10-09.

- Removed `FileIo`'s custom `write_all` override. The standard
  [`Write::write_all`](https://doc.rust-lang.org/std/io/trait.Write.html#method.write_all)
  loop now calls the existing single-wave `write` adapter, which still uses
  `Vfsi::vwrite`. Successful short writes advance the cursor before the next
  attempt; `Interrupted` retries only the unacknowledged suffix.
- Transport failures and other non-interruption errors stop completion without
  replay. No public API or duplicate completion engine was added.
- Added regression coverage for interruption before progress and after one or
  two short writes, for positional and append handles. Tests assert exact file
  contents, final cursor, and backend call counts. Additional coverage checks
  `WriteZero`, empty completions, and retained cursor progress after semantic
  and ambiguous transport failures.
- The new interruption and zero-progress tests failed before the fix. All 55
  native-client tests, the full `scripts/test-rust.sh` suite, workspace Clippy
  with all targets/features and warnings denied, formatting, and diff checks
  pass after the fix. Live NFS/SMB suites were not rerun for this adapter fix.
