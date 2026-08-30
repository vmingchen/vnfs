# nfs4fs

An `fsspec` filesystem (`nfs4://`) backed by the vectorized
[`vnfs`](../vnfs) NFSv4.1 client.

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

For tests without an NFS server, use `backend="dummy"` (a local-directory
implementation of the same vectorized API):

```python
fs = fsspec.filesystem("nfs4", backend="dummy", dummy_root="/tmp/scratch")
```
