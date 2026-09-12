# vsmbfs

`vsmbfs` is the fsspec adapter for the low-level `vsmb` vector SMB2/3 client.

```sh
python -m pip install vsmbfs
```

```python
import fsspec
import vsmbfs

fs = fsspec.filesystem(
    "vsmbfs",
    host="files.example",
    share="data",
    username="user",
    password="secret",
)
print(fs.ls("/"))
```

Bulk fsspec operations and coordinated `open_files` buffering use VFSI vector
operations so independent SMB requests can be issued in bounded batches.
