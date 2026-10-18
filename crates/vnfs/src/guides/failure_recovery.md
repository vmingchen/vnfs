# Failure, partial progress, and recovery

## Vectors are not transactions

An NFS COMPOUND executes in order until the first failing protocol operation.
Its prefix may have succeeded; its suffix was not executed. A logical vector
can span compounds, and a logical request can span chunks. Public vector
methods return all results on success or one structured `Error` on failure.
They do not roll back completed work.

`Error::index()` identifies a logical input when known, **not a committed-prefix
count**. A request at that index may have completed some chunks. Transport
failures can have unknown indices and ambiguous remote effects. Neither
status nor index alone proves that retrying a mutation is safe.

Use `kind()` for portable classification, `status()` and `domain()` for native
details, and `transport_kind()` for known timeout, connection, invalid-reply,
or authentication provenance. Unknown causes remain `Other`. `err_no()` is a
compatibility value, not universally a POSIX errno. Standard-I/O adapters
preserve the original error as an `io::Error` source.

## Short I/O and ownership

Range reads and default writes can return short progress. A short read without
EOF does not imply completion. Whole-file reads complete or fail rather than
truncate. `WriteOptions::new().with_write_all(true)` completes successful short
writes; it does not replay failed or ambiguous writes or guarantee durability.
Positional I/O preserves the handle cursor.

`ReadOp::into` borrows caller buffers only during `vread`. Results contain
offset, byte count, and EOF, but no borrowed buffers; `data()` is `None` for
these operations. Buffers can contain partial progress even on error.

`vclose(&mut files)` retains cleanup ownership on error and disarms handles
only after confirmed success. Consuming helpers instead fall back to
best-effort cleanup. Retained ownership does not establish remote open state.

## Automatic NFS recovery

The direct NFS client can reconnect and retry side-effect-free reads and
metadata queries once after supported transport or session failures. Reconnect
attempts use bounded exponential backoff through `NfsRecoveryPolicy`.
`set_auto_reconnect(false)` disables automatic recovery.

Recovery reopens retained NFS objects with batched `OPEN(CLAIM_FH)`, preserving
descriptor numbers, offsets, and independent open owners. Saved paths are
diagnostic: rename or pathname replacement cannot retarget recovery. Stale or
expired filehandles and revoked permissions still fail. Mount-derived clients
verify their pinned remote root and credentials, preserve read-only policy,
and do not revisit kernel mount discovery when reconnecting.

Python reader restoration creates new open instances by name after replacing
a session. It checks the complete `(fs_id, fileid)` pair; missing identity or
a different object fails closed. Rename or unlink can prevent that restoration.

Lost replies to create, write, rename, copy, remove, and other mutations are
not automatically replayed: the original operation may have succeeded.
Reconcile remote state before retrying. Streaming callbacks are not replayed
because they may have observed a prefix; resume from an application-defined
checkpoint. Reads and traversal are not snapshots of changing filesystems.

## Recursive removal and races

Path-taking removal can race with replacement of components before resolution.
The NFS remover traverses retained directory filehandles, but resolving the
initial directory remains a caller-side race. Path-only backend tokens are
not secure directory handles.

Use the path convenience API for directories whose removal you intentionally
own, with trusted ancestors:

```rust,no_run
use vnfs::files::VfsiExt;
use vnfs::directory::RemoveOptions;
# fn example(fs: &vnfs::nfs::NfsClient) -> vnfs::Result<()> {
fs.remove_dir_contents_with_options(
    "/owned-scratch-directory",
    RemoveOptions::new().with_batch_size(32),
)?;
# Ok(())
# }
```

The root must be a directory, not a symlink; child symlinks are removed without
following their targets. Each path call resolves the name anew. If a directory
was renamed, pass its intended current path; a saved pathname can now identify
a replacement object. A preflight attribute check does not eliminate namespace
races. The NFS path remover retains a directory filehandle internally for the
duration of removal, not across separate application calls.

Workflows that explicitly require a retained directory identity use the core
`Vfsi::vopen_dirs`/`vremove_dir_contents` operations on a supported backend.
Path-only tokens must be rejected by those operations, and opening a genuine
handle still does not secure attacker-controlled ancestors. The scalar
extension API uses paths rather than exposing another handle workflow.

`RemoveOptions` selects fail-fast/best-effort behavior, batch caps, and
transient-entry retries on supported backends. Generic removers reject
non-default tuning rather than ignore it.
