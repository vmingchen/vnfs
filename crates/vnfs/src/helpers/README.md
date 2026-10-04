# Rust workflow helpers

`vnfs::helpers` contains high-level workflows built only on the public `Fs`
and `FileHandle` contracts. It is part of `vnfs`, not a separate dependency.

## TreeBuilder

```rust,no_run
use vnfs::{FsExt, Nfs, helpers::TreeBuilder};

let client = Nfs::connect("server.example.com")?;
let tree = TreeBuilder::new()
    .add_file("config/app.conf", "host = localhost")
    .add_empty_file("logs/app.log")
    .add_directory("data/raw")
    .create(&client, "/new-workspace")?;

// Optional, explicit cleanup. Dropping `tree` does not delete anything.
client.remove_dir_all_one(tree.root())?;
# Ok::<(), vnfs::Error>(())
```

The same builder accepts mounted/routed clients through `Fs`. Paths are
relative to the tree root in that client's namespace. The root's parent must
exist; the root itself must not. Entries cannot escape lexically through `..`
or absolute paths. This is not protection against concurrent namespace changes
or symlinks in the root's ancestors: use trusted parents.

Planning performs no I/O. It validates all entries and infers each parent once.
Creation batches directories by depth, then files through vector OPEN,
WRITE-all, and CLOSE. File contents remain borrowed from the plan during writes.
`create_dirs` is also available directly on clients for bulk directory creation
when the parents already exist.

Defaults: 64 entries per batch, 10,000 entries (including inferred parents),
16 MiB of contents plus planned paths, and at most 128 path components.
Configure `batch_size`, `max_entries`, and `max_total_bytes` before adding entries.
Backend negotiated compound limits can split batches further.

Creation is not transactional. An I/O failure can leave a partial tree; it is
not silently retried, rolled back, or deleted. Errors retain protocol status,
path, and the original declaration index. Explicit directories can be repeated,
but duplicate files and file/directory collisions are rejected before I/O.

YAML loading, automatic temporary-root selection, file permissions, and cleanup
guards are intentionally not included in this initial helper.

Tests cover local semantics and injected partial failures. The live NFS test
compares compound counts against the same tree created with `batch_size(1)`:

```sh
VFSI_NFS_SERVER=127.0.0.1:2049 VFSI_NFS_REQUIRED=1 \
  cargo test -p vnfs --test tree_builder_nfs -- --nocapture
```

Set `VFSI_NFS_MINOR=1` or `2` to test just one minor version. CI exercises both
in its existing NFS integration matrix.
