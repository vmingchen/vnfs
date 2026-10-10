# Standard Rust I/O and handle lifecycle

Import `vnfs::prelude::*` for vector primitives (`Vfsi`) and conveniences
(`VfsiExt`). Prefer vectors for independent work on multiple files; scalar
helpers support interoperability and one-file workflows.

`NfsClient` is cheaply cloneable; clones share a synchronized connection, not
independent RPC parallelism. Owned handles can coexist and move to worker
threads. Handles expose lifecycle operations; I/O and metadata go through the
filesystem traits.

## The standard-I/O adapter

`fs.std_io(&file)` borrows the filesystem and handle and implements `Read`,
`Write`, and `Seek` through the vector engine. Each new adapter starts at offset
zero. Retain the same adapter to preserve its cursor; use `stream_position()`
to inspect it. Its concrete type is intentionally private.

```rust,no_run
use std::io::{Read, Seek};
use vnfs::prelude::*;

fn main() -> std::io::Result<()> {
    let fs = Nfs::connect("nfs.example.com")?;
    let file = fs.open("/file-1")?;
    let mut contents = Vec::new();
    {
        let mut io = fs.std_io(&file);
        io.read_to_end(&mut contents)?;
        assert_eq!(io.stream_position()?, contents.len() as u64);
    }
    file.close()?;
    Ok(())
}
```

Collecting reads enforce the client's read budget. At the limit, `read_to_end`
may consume an additional byte to distinguish EOF from overflow. Failure
does not roll back the cursor or remove already appended bytes. Repeated
reads into caller-managed buffers do not impose a total-stream allocation cap.

`Write::flush` requests backend durability (`sync_data`), not merely flushing a
user-space buffer, and can cost a network round trip. Call `fs.sync_data(&file)`
or `fs.sync_all(&file)` explicitly when durability errors matter.

## Opening and closing

The shared `fs.open_options()` builder provides scalar `open` and vector
`vopen`, both built on `Vfsi::vopen`.

Use `file.try_close()` or `fs.vclose(&mut files)` to retain local cleanup
ownership on failure. Consuming `file.close()` surfaces the error but leaves
cleanup to Drop on failure; `close_files` similarly consumes a group. Drop
is best-effort, may perform or defer cleanup, and cannot report errors. An
armed handle after a failed close is not proof that it remains open remotely.

Vector errors retain protocol details. Standard-I/O adapters wrap them in
`std::io::Error`, retaining the original error as its source. Sparse attribute
getters such as `Attrs::len()` return `None` for missing fields; missing does
not mean zero.

See [failure and recovery](crate::guides::failure_recovery) and
[resource limits](crate::guides::operations).
