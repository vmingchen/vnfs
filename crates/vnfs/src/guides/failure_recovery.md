# Failure, partial progress, and recovery

## Vectors are not transactions

An NFS COMPOUND executes in order until the first failing protocol operation.
Its prefix may have succeeded; its suffix was not executed. A logical vector
can span compounds, and a logical request can span chunks. Public vector
methods return `VecResult<T>`: an outer `std::io::Error` rejects the call before
dispatch; otherwise every input has its own value or `OpError`. Result positions
preserve input order, while execution order is backend-defined. Completed work
is not rolled back, and an ordinary item error does not cancel independent inputs.

An `OpError` retains an index-free standard error, `FailureState`
(`NotAttempted`, `Failed`, or `Unknown`), and optional `IoProgress` describing
acknowledged read or write progress. Neither a result position nor an error
category proves that retrying a mutation is safe. A failed request can follow
successful chunks of that same request.

Use the cause's `kind()` for portable classification,
`vnfs::error::native_status(cause)` for optional typed errno/NFS/SMB details, and
`transport_kind(cause)` for known timeout, connection, invalid-reply, or
authentication provenance. `raw_os_error()` is not a protocol status accessor.
Unknown foreign causes remain valid standard errors without invented native facts.

Strict `VfsiExt` helpers such as `read_batch` and `listdirs_all` select the first
failed input after execution and return `IndexedError`. Its `index()` is helper
attribution, not a completed-prefix count; it does not retain certainty, progress,
or sibling outcomes. Keep `VecResult` when those details matter. Singular helpers
and standard-I/O adapters return `std::io::Error`.

## Short I/O and ownership

Range reads and default writes can return short progress. A short read without
EOF does not imply completion. Whole-file reads complete or fail rather than
truncate. `WriteOptions::new().with_write_all(true)` completes successful short
writes; it does not replay failed or ambiguous writes or guarantee durability.
Positional I/O preserves the handle cursor.

`ReadOp::into` borrows caller buffers only during `vread`. Results contain
offset, byte count, and EOF, but no borrowed buffers; `data()` is `None` for
these operations. Buffers can contain partial progress even on error.

`vclose(&mut files)` returns one outcome per input and disarms every
confirmed success, even when a sibling fails. Failed or uncertain inputs
retain cleanup ownership; already closed inputs succeed without dispatch.
`close_batch` selects the first error in input order after the batch;
`close_files` consumes the handles and falls back to best-effort cleanup.
Retained ownership does not establish remote open state.

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
    RemoveOptions::new().with_batch_size(std::num::NonZeroUsize::new(32)),
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
