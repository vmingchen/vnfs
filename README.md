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

[fast]: https://www.usenix.org/conference/fast17/technical-sessions/presentation/chen
[vnfs_client]: https://github.com/sbu-fsl/fsl-tc-client
[rfcv4]: https://datatracker.ietf.org/doc/html/rfc7530#page-170
[rfcv4_2]: https://datatracker.ietf.org/doc/html/rfc7862