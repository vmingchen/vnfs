# vfsi-fsspec

`vfsi-fsspec` is the backend-neutral buffering, batching, caching, callback,
and transaction engine shared by the `nfs4fs` and `vsmbfs` distributions. Most
users should install one of those protocol packages rather than this package
directly.

Upgrade the protocol package (`nfs4fs` or `vsmbfs`) together with
`vfsi-fsspec`. The shared engine requires native `read_all_many` support for
per-call byte budgets and rejects older extensions before connecting; it never
falls back to an unbounded read.
