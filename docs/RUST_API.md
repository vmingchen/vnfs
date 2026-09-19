# Rust API architecture

The Rust-native VFSI API is intentionally separated from the historical
POSIX/C compatibility surface.

- `FileSystem` is the descriptor I/O contract. `MetadataFileSystem`,
  `DirectoryFileSystem`, `NamespaceFileSystem`, `LinkFileSystem`, and
  `CopyFileSystem` add focused capabilities; `NativeFileSystem` is their
  convenient aggregate bound.
- `VectorFileSystem` adds optimized ordered batches.
- `FsClient` owns and shares a backend; `FsFile` owns a remote handle without
  borrowing the entire client.
- `NfsExtensions` and `SmbExtensions` contain protocol-only negotiated state.
- `OpenRequest`, `MetadataQuery`, and `SetAttributes` replace raw flags and
  overloaded metadata masks.
- Public `*v` methods return all values or one indexed `VfError`. They do not
  promise rollback; a transport failure is never presented as an ordinary
  filesystem status.
- `Capabilities` is a typed bitset. Integer `VF_CAP_*` constants remain for
  source and C ABI compatibility.
- `NfsClientBuilder` configures namespace root, protocol version, timeouts,
  authentication, recovery, observability, and compound limits before
  connecting.
- `NfsVecFs::shutdown` reports close/session teardown errors; `Drop` remains a
  best-effort safety net.

## Compatibility boundary

`VecFs`, `VfFile`, `Fd`, `VfAttrs`, `VfOpenOptions`, and raw libc flags remain
available while existing C and Python bindings migrate. Raw NFS protocol
modules are available only under `vnfs::legacy`. New application code should
start with:

```rust
use vnfs::prelude::*;
```

The native owned-file API never exposes its backend descriptor. Vector
requests made through `FsClient` verify that every file belongs to that same
client, preventing accidental cross-session descriptor use.

Application code should connect through `Nfs::builder`, which directly
returns the concrete `NfsClient` alias. `NfsVecFs` and `NfsClientBuilder`
remain available for embedding and compatibility. `NfsClient::open_options`
mirrors `std::fs::OpenOptions`; direct `read_at` and `write_at` perform
positional I/O, while explicitly named `read_request_at` and
`write_request_at` values compose vector calls. `closev` consumes a group
of handles and closes them with the vector backend rather than serializing
one close per dropped handle.

## Durability and failure rules

`flush`, `sync_data`, and `sync_all` call the backend durability operation.
NFS requests `FILE_SYNC4`; SMB issues `FLUSH`; the local backend calls the
corresponding `std::fs::File` method.

Semantic compound errors prove that a prefix completed and the suffix was not
attempted. A lost transport response leaves dispatched mutations indeterminate;
they are not replayed. `VfError` exposes its status domain, optional request
index and operation/path context, completion certainty, and retry class without
requiring string parsing.

Native client and file methods retain `VfError`. Only the standard-library
`Read`, `Write`, and `Seek` adapters translate failures to `std::io::Error`.

The asynchronous facet is deliberately deferred to a separate future crate;
the synchronous core does not depend on Tokio.
