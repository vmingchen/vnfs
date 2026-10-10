# vNFS: fewer round trips for many files

`vnfs` is a synchronous Rust client for NFSv4.1 and NFSv4.2. Use familiar
scalar operations for one file, and **submit independent work together** when
processing many files or directories. The backend packs vectors into NFSv4
COMPOUND requests within the server's negotiated operation and byte limits.
You do not need to construct protocol operations yourself.

## Start here

```no_run
# #[cfg(feature = "nfs")]
# fn main() -> vnfs::Result<()> {
use vnfs::{Vfsi, VfsiExt, Nfs, ReadOp};

let fs = Nfs::builder("nfs.example.com")
    .root("/export/application")
    .connect()?;
// Replaces these files; their parent must already exist.
fs.write_files(&[
    ("/file-1", b"hello".as_slice()),
    ("/file-2", b"world".as_slice()),
])?;
let results = fs.vread([
    ReadOp::whole("/file-1"),
    ReadOp::whole("/file-2"),
], Default::default())?;
assert_eq!(results[0].data(), Some(b"hello".as_slice()));
assert_eq!(results[1].data(), Some(b"world".as_slice()));
# Ok(())
# }
# #[cfg(not(feature = "nfs"))]
# fn main() {}
```

Both files share vector phases instead of a scalar loop. `write_files` performs
OPEN, complete WRITE, and CLOSE phases; it is **not one RPC or a transaction**.
Small path-based reads can share a single compound when negotiated limits
permit. Larger batches are split automatically, and successful results retain
input order. See [the runnable examples](examples) for complete workflows.

Paths are relative to the configured remote namespace: `/file-1` above names
`/export/application/file-1`, not a host-local file. This root is not a security
sandbox. Linux native build prerequisites and measured small-file benchmarks
are in the [package README](https://github.com/vmingchen/vnfs/tree/main/crates/vnfs#readme).

## Choose the API by task

| Task | Recommended API | Example |
| --- | --- | --- |
| Many complete small files | [`Vfsi::vread`], [`VfsiExt::write_files`] | [Bulk files](examples::bulk_files) |
| Repeated or positional I/O | [`Vfsi::vopen`], [`Vfsi::vread`], [`Vfsi::vwrite`] | [Open handles](examples::open_handles) |
| One large file | [`Vfsi::vstream`] | [Bounded streaming](examples::stream_file) |
| Many directory listings with attributes | [`Vfsi::vlistdirs`] | [Directories](examples::directories) |
| Large trees without collecting everything | [`Vfsi::vlistdirs`] with [`ListDirOptions::recursive`] | [Directories](examples::directories) |
| Declarative fresh directory tree | `helpers::TreeBuilder` | [Builder example](helpers::TreeBuilder) |
| Existing Linux NFS mount | `NfsMountSession::from_mount` for a connection plus local-path mapping; `Nfs::from_mount` for a connection only | [Mount discovery](Nfs::from_mount) |
| Backend-independent application code | `Vfsi`, `VfsiExt`, `FileHandle` | [Generic workflows](examples) |

`Vfsi` and `VfsiExt` are defined in `vfsi-core` and re-exported here;
applications using `vnfs` need no additional dependency or import path.
`Vfsi` contains vectorized execution primitives. `VfsiExt` supplies scalar operations and
convenience helpers (`read_files`, `write_files`, scalar open, and default-option
listing/streaming) without scalarizing vectors. `VfsiExt::read_dirs_with_options`
collects pages returned by `Vfsi::vlistdirs`; `VfsiExt::read_stream_with_options`
is the single-path adapter for `Vfsi::vstream`. These remain valid extension
helpers, not core execution methods. Import both with `vnfs::prelude::*`.

## API map

- [`guides`]: standard Rust I/O, failure/recovery, authentication, and operational tuning.
- [`nfs`]: direct connections, owned handles, authentication, read pools, tuning.
- [`files`]: portable I/O traits, read/write results, resource limits.
- [`directory`]: metadata selection, listings, traversal and removal options.
- [`error`]: error kinds, protocol statuses, and failing vector indices.
- [`helpers`]: high-level workflows such as creating a directory tree.
- `mounted` (Linux, `auto` feature): kernel access and automatic routing, **not** coherent caching
  between kernel and direct clients.
- [`diagnostics`]: optional process-wide compound/RPC counters.
- [`prelude`]: common imports. Common application types also remain at the root.
- Separate `vfsi-*` crates: advanced backend implementation; ordinary
  applications should not need these implementation crates.

## Guarantees and operational choices

- Vector calls are strict, ordered, and **non-atomic**. An error can follow
  completed mutations. Its input index is not a committed-prefix count; do not
  blindly replay writes after an ambiguous transport failure. Inspect `Error`
  using [`error`] and preserve it when reporting failures.
- `vread` defaults to the client’s 16 MiB aggregate byte budget.
  Adjust [`ResourceLimits`] or [`ReadOptions`],
  or stream instead. Directory collection and traversal have separate budgets.
  These are not a process-wide peak-memory cap. The explicit `std_io` adapter bounds collecting reads; caller-managed buffering
  does not inherit an allocation limit.
- `vread` range requests and `vwrite` may return short progress. Whole-file
  requests complete or fail; they never silently truncate. Use `vwrite`
  with `WriteOptions::new().write_all(true)` to complete
  successful short writes; do not mistake a short read without EOF for completion.
  Positional requests preserve the file cursor.
- Files close best-effort on drop. Use explicit `VfsiExt::close_files` to surface cleanup
  failures or `vclose` to retain local cleanup ownership on failure. Writes
  are not automatically durable; use `VfsiExt::sync_data` or `VfsiExt::sync_all` as needed.
- Cloning a client shares its session and lock, not independent parallelism.
  [`NfsClientPool`] distributes workloads across independent sessions;
  [`NfsReadPool`] provides bounded pipelined large-file reads.
- Default AUTH_SYS conveys Unix identity without cryptographic peer authentication
  or encryption. Enable `rpcsec-gss` and select [`NfsAuthentication`] explicitly
  for Kerberos. Requested secure authentication fails rather than downgrades.
- Direct NFS access, including mount discovery, does not share or invalidate
  the Linux kernel client's caches. Mixing the two requires an explicit coherence
  policy. Traversal and reads do not provide a filesystem snapshot.

## Cargo features

| Feature | Purpose |
| --- | --- |
| `nfs` (default) | Direct NFSv4 client |
| `server-copy` (default) | NFS server-side copy when supported |
| `auto` (default) | Linux mounted clients and conservative direct-NFS routing |
| `rpcsec-gss` | Optional Kerberos authentication or integrity protection |
| `posix` | Rooted kernel filesystem access through ordinary POSIX syscalls |
| `test-faults` | Fault injection for tests, not normal application use |

SMB is provided by the separate `vfsi-smb` library, not this crate.
The public API is pre-1.0 and may change between releases.
