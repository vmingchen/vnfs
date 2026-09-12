# vsmb

`vsmb` is the low-level Python client for the VFSI SMB2/3 backend. It exposes
both scalar operations and vector operations such as `stat_many`, `read_many`,
`write_many`, `open_many`, and descriptor-vector I/O.

```sh
python -m pip install vsmb
```

```python
from vsmb import SmbClient

with SmbClient(
    host="files.example",
    share="data",
    username="user",
    password="secret",
) as client:
    results, errors = client.stat_many(["/a", "/b", "/c"])
```

This package deliberately has no fsspec dependency. Install `vsmbfs` for the
fsspec-compatible filesystem API. Rust applications should use the separate
`vfsi-smb` crate.
