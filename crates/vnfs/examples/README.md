# Canonical Rust examples

Use the application API: no `vnfs::backend` or low-level `vfsi-*` imports are
needed. These examples are compiled in CI, exercised against mounted local
files, and included in the crate's rustdoc examples module.

Run from the repository root. `EXPORT_ROOT` is the server-visible directory;
subsequent paths are inside that namespace, not local mount paths.

| Example | Shows | Arguments after `--` |
| --- | --- | --- |
| `bulk_files` (also `demo`) | Complete vector writes/reads, fresh directory ownership, error cleanup | `HOST EXPORT_ROOT FRESH_DIRECTORY` |
| `open_handles` | Vector OPEN, positional reads into borrowed buffers, explicit CLOSE | `HOST EXPORT_ROOT FILE [FILE ...]` |
| `stream_file` | One large file, bounded chunks, no whole-file allocation | `HOST EXPORT_ROOT FILE` |
| `directories` | Vector listings with metadata; incremental no-follow walk | `HOST EXPORT_ROOT TREE [DIRECTORY ...]` |

```sh
cargo run -p vnfs --example bulk_files -- 127.0.0.1 /export /fresh-example
cargo run -p vnfs --example open_handles -- 127.0.0.1 /export /file-1 /file-2
cargo run -p vnfs --example stream_file -- 127.0.0.1 /export /large.bin
cargo run -p vnfs --example directories -- 127.0.0.1 /export /project
```

`bulk_files` creates and removes only a newly created directory. It fails if
the directory exists; it never pre-deletes an existing tree. Use a trusted
parent without concurrent renames or symlink substitution. Failed cleanup can
leave the example's directory behind. The other examples are read-only.

For declarative tree creation, see `vnfs::helpers::TreeBuilder` and its compiled
documentation example. For mount discovery, use
`vnfs::Nfs::from_mount("/absolute/mount/project")?`; this opens a separate
direct NFS connection, not a cache-coherent kernel handle. `Mounted` always
uses the kernel; `Auto` may bypass it. Generic example functions accept
`impl Vfsi` to share application logic across client types.

## Performance experiments, not introductory examples

`small_files_benchmark` and `large_file_read_benchmark` provide measurement
workloads and tuning options. Consult their command-line usage before running.
Vectors can span several compounds; use `vnfs::diagnostics` counters rather
than assuming one call means one RPC. Counters are process-wide. Cloning a
client does not create independent sessions.

## Verify the examples

`./scripts/test-rust.sh` runs the local workflow tests and example doctests.
Its `--quick` subset includes the local canonical workflows but skips doctests.
The NFS integration CI matrix runs the same example functions on NFSv4.1 and
v4.2. To run that test directly (without needing rpcbind discovery):

```sh
VFSI_NFS_SERVER=127.0.0.1:2049 VFSI_NFS_EXPORT=/ \
  cargo test -p vnfs --test canonical_examples \
  canonical_workflows_on_nfsv41_and_nfsv42 -- --ignored --test-threads=1
```

The export must be writable. Set `VFSI_NFS_MINOR=1` or `2` to select only one
protocol; otherwise both are exercised. The live test is ignored in the
server-independent suite, not silently considered tested without a server.
