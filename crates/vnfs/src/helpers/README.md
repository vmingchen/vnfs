# Rust workflow helpers

`vnfs::helpers` contains high-level workflows built only on the public `Vfsi`
and `FileHandle` contracts. It is part of `vnfs`, not a separate dependency.

## Application bridges

`helpers::MountSession` pairs one reusable client with its local namespace root.
Use `ResolvePath::Follow` for existing source operands and `NoFollow` for links,
removal operands and new destinations. Mapping preserves non-UTF-8 paths and
rejects operands outside the session root. It is not race-free confinement or a
kernel/direct-client cache-coherence mechanism. Keep mount configuration stable
for the session's lifetime.

`VfsiExt::visit_dirs_ordered` provides lazy, application-ordered directory
listings with entry/path/depth budgets and admission before child-directory I/O.
It lets tools such as `ls` retain their sort and display policy without owning a
second traversal engine. Use `walk_events_with_options` for pre/post-order entry
events and `vlistdirs` to batch independent, already-approved directories.

`helpers::copy_to_writer` streams a source to a caller-owned `std::io::Write`
destination. It handles short writes, uses bounded read chunks, and stops on
writer failure without replay. Opening/truncating, publication, metadata and
flushing remain application policy. For a shared filesystem destination, prefer
the existing vectorized `copy_items` workflows.

```rust,no_run
use vnfs::{ReadStreamOptions, helpers::{MountSession, ResolvePath, copy_to_writer}};

let session = MountSession::from_mount("/mnt/data")?;
let source = session.map("/mnt/data/input", ResolvePath::Follow)?;
let mut output = std::fs::File::create("output")?;
copy_to_writer(session.fs(), source, &mut output,
    ReadStreamOptions::new().chunk_size(1024 * 1024))?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## TreeBuilder

```rust,no_run
use vnfs::{VfsiExt, Nfs, helpers::TreeBuilder};

let client = Nfs::connect("server.example.com")?;
let tree = TreeBuilder::new()
    .add_file("config/app.conf", "host = localhost")
    .add_empty_file("logs/app.log")
    .add_directory("data/raw")
    .create(&client, "/new-workspace")?;

// Optional, explicit cleanup. Dropping `tree` does not delete anything.
client.remove_dir_all(tree.root())?;
# Ok::<(), vnfs::Error>(())
```

The same builder accepts mounted/routed clients through `Vfsi`. Paths are
relative to the tree root in that client's namespace. The root's parent must
exist; the root itself must not. Entries cannot escape lexically through `..`
or absolute paths. This is not protection against concurrent namespace changes
or symlinks in the root's ancestors: use trusted parents.

Planning performs no I/O. It validates all entries and infers each parent once.
Creation batches directories by depth, then files through vector OPEN,
WRITE-all, and CLOSE. File contents remain borrowed from the plan during writes.
`vmkdir` is also available directly on clients for bulk directory creation
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

## Copy, move, and tree statistics

```rust,no_run
use vnfs::{Nfs, VisitOptions, helpers::{copy_items, tree_stats, CopyOptions}};

let fs = Nfs::connect("server.example.com")?;
let result = copy_items(&fs, &["/input/images", "/input/config"], "/output",
    CopyOptions::new().batch_size(64).chunk_bytes(1024 * 1024))?;
println!("copied {} files", result.files_copied);
let stats = tree_stats(&fs, "/output", VisitOptions::new())?;
println!("{} files, {} logical bytes", stats.files, stats.file_bytes);
# Ok::<(), vnfs::Error>(())
```

`copy_items` accepts mixed file/directory roots and defaults to placing their
basenames under the destination directory. `CopyLayout::Contents` places each
directory's children directly there; regular files still keep their basenames.
`copy_tree` maps one directory to an exact destination root. Destination parents
must exist. Directory pages are consumed lazily, with bounded OPEN/READ/WRITE/
CLOSE vectors. Read storage is bounded by the client's `max_read_bytes`, chunks
and batch size; entry/path/depth limits bound traversal. There is no eager size
prewalk. Relative paths start at the client's root, not the process's working
directory; `..`, lexical overlaps and duplicate container destinations are rejected.

Files default to exclusive creation (`Existing::Error`). `Existing::Skip` retains
existing files; its destination reservations are necessarily scalar because a
failed strict vector OPEN cannot identify which files it created. `Existing::Replace`
unlinks existing regular files before exclusive creation, protecting unrelated
hard links from truncation. It selects native vector COPY when neither progress
nor permission preservation is requested. Native COPY has no portable byte-count
result, so `bytes_copied` is `None`, not an estimate. Links and special objects
are rejected unless explicitly skipped. New files initially use mode 0600;
`preserve_permissions(true)` applies source file permission bits after completion
and fails if the backend cannot supply/apply them. It does not preserve directory
modes, ownership, timestamps, ACLs, sparse layout or hard-link topology.

`copy_items_with_progress` and `copy_tree_with_progress` accept a synchronous,
fallible callback returning `ControlFlow`. Byte progress counts accepted writes,
not durability; directory events have zero file sizes/bytes. Stop takes effect
after the current vector wave, so sibling writes may already have completed.
Descriptor copies stop at the initially observed file size; growth is ignored.

`move_items` and its progress variant delete a root only after copying and
explicit CLOSE succeed, without skips or cancellation. `Container + Replace`
without progress or depth overrides can rename an absent destination; only an
explicit cross-device error enables copy/delete fallback. Other modes copy
first, preserving exclusive conflict checks. Failure, cancellation, or skipped
entries retains the source (which can leave two copies). No ambiguous write or
rename is replayed by these helpers.

`Vfsi::vrename_with_options` also offers `RenameOptions::NoReplace` for callers
that need an atomic absent-destination check. The Linux `Mounted` backend uses
`renameat2`; direct NFS and SMB backends currently return `Unsupported` because
their exposed rename operations cannot promise that guarantee. `move_items`
falls back to its copy/delete path only after an explicit Unsupported, EEXIST, or
EXDEV response and reconciles every source/destination pair first. A lost
transport reply is never retried. Exchange rename semantics are not exposed yet.

These are nontransactional workflows over **trusted, stable namespaces**. They
do not sandbox ancestor symlinks or concurrent namespace changes, snapshot files,
rollback output, or guarantee durability. Replacement failures can leave missing
or partial destinations. Use quiescent source trees, particularly for moves.

`tree_stats` folds recursive directory pages without materializing the tree or
issuing per-child stats. It reports logical bytes, counts hard links per name,
does not follow symlinks, and includes the root directory. VisitOptions controls
its depth/entry/path budgets; explicitly truncated depth reports only that portion.
