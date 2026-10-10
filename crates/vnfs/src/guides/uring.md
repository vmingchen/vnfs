# Local io_uring backend

Enable the optional Linux backend without bringing in the NFS native library:

```toml
[dependencies]
vnfs = { path = "/path/to/vnfs/crates/vnfs", default-features = false, features = ["uring"] }
```

This feature is currently in the development checkout, not yet published.

```no_run
use vnfs::{Uring, Vfsi, VfsiExt, OpenOp, OpenFlags, ReadOp};

# fn main() -> vnfs::Result<()> {
let fs = Uring::new("/data")?;
let files = fs.vopen(&[
    OpenOp::new("/file-1", OpenFlags::READ),
    OpenOp::new("/file-2", OpenFlags::READ),
])?;
let result = fs.vread(files.iter().map(|file| ReadOp::range(file, 0, 4096)),
                     Default::default());
let close = fs.close_files(files);
let results = result?;
close?;
for result in results { println!("{} bytes", result.read()); }
# Ok(())
# }
```

`vfsi-uring::connect` provides the same `Vfsi`/`VfsiExt` API as a standalone
crate. `vnfs::Uring` keeps backend handles opaque. Neither needs Tokio.

## Execution and limits

Independent descriptor reads, positional writes, and fsyncs are submitted as
multiple SQEs per wave. Completion IDs restore input order. Default bounds:
256 SQEs and 16 MiB scratch per wave, configurable through
`vnfs::uring::Options::{queue_depth,max_batch_bytes}`. Queue depths must be
1..=4096. Application read budgets remain independently controlled by
`ReadOptions` and `ResourceLimits` (16 MiB by default). Large requests use bounded
contiguous chunks; short writes complete their remaining suffix, never skip gaps.

Open/close, metadata, directories, namespace changes, append and dependent writes
retain the existing local kernel implementation. This is deliberately **not**
an all-opcode backend. `write_files` still batches its descriptor-write phase.
Repeated cursor reads and overlapping writes, including hard-link aliases,
preserve ordering rather than racing. Nothing promises batch atomicity.

All completions are drained before returning an indexed CQE failure; other
independent writes in the submitted wave may already have completed. No later
wave or remaining short-write suffix is submitted after a known failure. Ring-level
submission failures poison the executor and do not replay writes. If completion
draining becomes impossible, the bounded in-flight wave's owned buffers and
shared file descriptors are deliberately retained rather than freed while the
kernel may still reference them. This exceptional path leaks bounded resources;
replace the client after such an error. Setup restrictions (kernel policy,
seccomp, unavailable opcodes) return an error, not silent syscall fallback.

This backend uses buffered I/O with an owned scratch copy to make submission
failure safe. It is not zero-copy, O_DIRECT, SQPOLL, registered-buffer I/O, or an
asynchronous API. `vread` caller buffers avoid application-owned output vectors,
but still pass through bounded kernel-facing scratch. Writes alone do not imply
durability; call `vfsync` when required.

`Uring::with_telemetry` returns counters for waves, actual submissions,
completions, and peak scratch bytes. They exclude the ordinary syscall paths.

## Reproduce the comparison

```sh
./scripts/test-ci-local.sh uring
cargo run --release -p vnfs --no-default-features --features uring,dummy \
  --example uring_bench -- 200
```

An optional second argument selects an existing scratch parent directory.
The default is `target`, because `/tmp` can be RAM-backed. The benchmark uses
identical `FsClient` adapters over the same root, comparing the ordinary local
executor (`DummyVecFs`, real local files) against the ring executor. It validates
distinct file/range patterns, poisons read buffers before each call, changes
write payloads every round, and independently reads written files through the
standard filesystem API. Validation and source reset stay outside timing. It
keeps opens/closes outside timed I/O, warms up five rounds, and reports
median per-vector time. Directory listings retain the same implementation.
Writes exclude fsync. Advisory-cold reads use `POSIX_FADV_DONTNEED` after syncing,
outside timing; this does not evict device/controller caches or guarantee cold
physical media. See `docs/uring-benchmark.md` in the repository for results and
environment. Do not expect io_uring to win on every workload.
