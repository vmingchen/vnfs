# vfsi-fsspec

`vfsi-fsspec` is the backend-neutral buffering, batching, caching, callback,
and transaction engine shared by the `nfs4fs` and `vsmbfs` distributions. Most
users should install one of those protocol packages rather than this package
directly.

Upgrade the protocol package (`nfs4fs` or `vsmbfs`) together with
`vfsi-fsspec`. The shared engine requires native `read_all_many` support for
per-call byte budgets and rejects older extensions before connecting; it never
falls back to an unbounded read.

Partial open-ended `cat_file` reads and all open-ended `cat_ranges` reads use
the EOF observed during size validation as their exclusive end. Later growth
is excluded from that read, so it cannot exceed the checked allocation budget;
truncation may produce a short read. This bounds the range, not a snapshot of
its contents.
Whole-file `cat_file` reads (normalized start zero, no end) retain the native
read-to-EOF fast path, which enforces the allocation budget while reading.

Transactional writes require both a native creation-permissions API and a backend
with POSIX permission support. Unsupported backends, including current SMB,
reject transactional writes before allocating a spool or creating staging files;
ordinary I/O remains supported. A method signature alone is not a capability.


### Independent native I/O outcomes

Recent native modules provide `preadv`, `pwritev`, `appendv` and
`read_allv`. These return one success value or exception per input. Exceptions
retain `failure_state` (`not_attempted`, `failed` or `unknown`) and optional
`read_prefix`/`read_offset`/`read_eof` or `bytes_written`. Unsupported inputs
use the existing `NotImplementedError` class. A failed prefix is not a successful
operation; an unknown mutation must not be replayed.

Grouped buffered I/O uses these methods when available: healthy reads are kept,
only failed read suffixes can recover, and healthy writes continue through later
waves. Old native modules retain their existing path. The older `*_many` native
methods keep their return shapes and exception behavior as compatibility projections.
