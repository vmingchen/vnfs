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
use vnfs::Nfs;

let fs = Nfs::builder("nfs.example.com")
    .root("/export/application")
    .connect()?;
// Replaces these files; their parent must already exist.
fs.write_files(&[
    ("/file-1", b"hello".as_slice()),
    ("/file-2", b"world".as_slice()),
])?;
let contents = fs.read_files(&["/file-1", "/file-2"])?;
assert_eq!(contents, [b"hello".to_vec(), b"world".to_vec()]);
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
| Many complete small files | `NfsClient::read_files`, `NfsClient::write_files` | [Bulk files](examples::bulk_files) |
| Repeated or positional I/O | `NfsClient::openv`, `NfsClient::readv_into`, `NfsClient::write_allv` | [Open handles](examples::open_handles) |
| One large file | `NfsClient::read_stream_with_options` | [Bounded streaming](examples::stream_file) |
| Many directory listings with attributes | `NfsClient::read_dirs_with_options` | [Directories](examples::directories) |
| Large trees without collecting everything | `NfsClient::visit_walk`, `NfsClient::walk_events_with_options` | [Directories](examples::directories) |
| Declarative fresh directory tree | `helpers::TreeBuilder` | [Builder example](helpers::TreeBuilder) |
| Existing Linux NFS mount | `Nfs::from_mount`, `NfsBuilder::from_mount` | [Mount discovery](Nfs::from_mount) |
| Backend-independent application code | `Client`, `FileHandle` | [Generic workflows](examples) |

## API map

- [`nfs`]: direct connections, owned handles, authentication, read pools, tuning.
- [`files`]: portable I/O traits, read/write results, resource limits.
- [`directory`]: metadata selection, listings, traversal and removal options.
- [`error`]: error kinds, protocol statuses, and failing vector indices.
- [`helpers`]: high-level workflows such as creating a directory tree.
- `mounted` (Linux, `auto` feature): kernel access and automatic routing, **not** coherent caching
  between kernel and direct clients.
- [`diagnostics`]: optional process-wide compound/RPC counters.
- [`prelude`]: common imports. Common application types also remain at the root.
- [`backend`]: advanced backend implementation and protocol construction; ordinary
  applications should not need this module.

## Guarantees and operational choices

- Vector calls are strict, ordered, and **non-atomic**. An error can follow
  completed mutations. Its input index is not a committed-prefix count; do not
  blindly replay writes after an ambiguous transport failure. Inspect `Error`
  using [`error`] and preserve it when reporting failures.
- `read` and `read_files` default to a 16 MiB returned-data budget (aggregate
  across files for `read_files`). Adjust [`ResourceLimits`] or [`ReadAllOptions`],
  or stream instead. Directory collection and traversal have separate budgets.
  These are not a process-wide peak-memory cap. Standard `std::io::Read::read_to_end`
  does not inherit an allocation limit.
- `readv` and `writev` may return short progress. Use `write_allv` to complete
  successful short writes; do not mistake a short read without EOF for completion.
  Positional requests preserve the file cursor.
- Files close best-effort on drop. Use explicit `closev` to surface cleanup
  failures or `try_closev` to retain local cleanup ownership on failure. Writes
  are not automatically durable; use a file's `sync_data` or `sync_all` as needed.
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
| `dummy` | Local backend for development/testing |
| `test-faults` | Fault injection for tests, not normal application use |

SMB is provided by the separate `vfsi-smb` library, not this crate.
The public API is pre-1.0 and may change between releases.
