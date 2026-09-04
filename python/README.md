# nfs4fs

An `fsspec` filesystem (`nfs4://`) backed by the vectorized
[`vnfs`](../vnfs) NFSv4.1 client.

`vnfs` minimizes network round trips by batching many filesystem operations
into one NFSv4 compound. Instead of paying a round trip for each
`open`/`read`/`write`/`close` (or each file), fsspec bulk calls such as
`pipe` and `cat` submit their operations through one vectorized vnfs call and
one NFSv4 compound per batch.

The Rust extension (`nfs4fs._native`) links libntirpc **statically**, so
`import nfs4fs` works without `LD_LIBRARY_PATH`.

## Build

```sh
python3 -m venv .venv
.venv/bin/pip install maturin fsspec pytest
cd python
../.venv/bin/maturin develop
```

## Use

```python
import fsspec
import nfs4fs  # registers the "nfs4" protocol

fs = fsspec.filesystem("nfs4", host="127.0.0.1", root="git/some/tree")
fs.pipe({"nfs4:///a.txt": b"hello", "nfs4:///b.txt": b"world"})
print(fs.cat(["nfs4:///a.txt", "nfs4:///b.txt"]))
```

The round-trip difference is easy to see in that example. A conventional
per-file NFS client needs roughly three round trips per file per direction
(resolve/open, the read or write itself, and close), so about six round trips
for the `pipe` of two files and another six for the `cat`. With nfs4fs, vnfs
puts the lookups, opens, truncate+write, and closes for both files into one
compound (one `writev`), and `read_allv` reads both files in one compound.
The whole example therefore uses **2 network round trips** (1 RTT per bulk
call).

For tests without an NFS server, use `backend="dummy"` (a local-directory
implementation of the same vectorized API):

```python
fs = fsspec.filesystem("nfs4", backend="dummy", dummy_root="/tmp/scratch")
```
