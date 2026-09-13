# Rust API architecture

The Rust-native VFSI API is intentionally separated from the historical
POSIX/C compatibility surface.

- `FileSystem` is the scalar contract.
- `VectorFileSystem` adds optimized ordered batches.
- `FsClient` owns and shares a backend; `FsFile` owns a remote handle without
  borrowing the entire client.
- `NfsExtensions` and `SmbExtensions` contain protocol-only negotiated state.
- `OpenRequest`, `MetadataQuery`, and `SetAttributes` replace raw flags and
  overloaded metadata masks.
- `BatchOutcome` preserves completed, failed, unexecuted, and ambiguous
  operations. A transport failure is never presented as an ordinary status.
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

## Durability and failure rules

`flush`, `sync_data`, and `sync_all` call the backend durability operation.
NFS requests `FILE_SYNC4`; SMB issues `FLUSH`; the local backend calls the
corresponding `std::fs::File` method.

Semantic compound errors prove that a prefix completed and the suffix was not
attempted. A lost transport response makes dispatched mutations
`Indeterminate`; they are not replayed. `VfError` exposes its status domain,
optional operation/path context, completion certainty, and retry class without
requiring string parsing.

The asynchronous facet is deliberately deferred to a separate future crate;
the synchronous core does not depend on Tokio.
