# Vectorized NFS client

## Intro

Network File System (NFS) is a IETF standard of performing file operations over
network. NFS starting from [version 4.0][rfcv4] supports compounds that can
coalesce many small file operations into one big NFSv4 compound to save network
round trips. NFSv4 has multiple minor versions; the latest minor version is
[NFSv4.2][rfcv4_2].

However, the coalescing capability of NFSv4 is not easily accessible by
applications because many applications usually uses the POSIX file-system API
which operates on each individual file operation. To solve this problem, one
idea is to use provide an alternative API using vectorization. The idea is
researched by the FAST'17 paper [vNFS: Maximizing NFS Performance with
Compounds and Vectorized I/O][fast].

The [original vNFS client and library][vnfs_client] were implemented in C. This
crates provides a Rust alternative.

Connections negotiate NFSv4.2 first and fall back to NFSv4.1 when necessary.
Optional server-side COPY is capability-aware and automatically falls back to
client-side reads and writes when a server does not implement it.

The same [`VecFs`](vnfs/src/vecfs.rs) interface also has an SMB2/3 backend for
Samba and other modern SMB servers. It negotiates SMB 2.0.2 through SMB 3.1.1,
uses related SMB compounds for small path operations, honors SMB credit and
I/O limits, and uses server-side copy with a client-side fallback.
The Rust, C, and Python/fsspec surfaces all expose this backend.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT), at your option.

[fast]: https://www.usenix.org/conference/fast17/technical-sessions/presentation/chen
[vnfs_client]: https://github.com/sbu-fsl/fsl-tc-client
[rfcv4]: https://datatracker.ietf.org/doc/html/rfc7530#page-170
[rfcv4_2]: https://datatracker.ietf.org/doc/html/rfc7862
