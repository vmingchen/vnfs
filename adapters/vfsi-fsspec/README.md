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
