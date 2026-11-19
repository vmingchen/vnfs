# vsmb

`vsmb` is the low-level Python client for the VFSI SMB2/3 backend. It exposes
both scalar operations and vector operations such as `stat_many`, `read_many`,
`write_many`, `open_many`, and descriptor-vector I/O.

```sh
python -m pip install vsmb
```

Published wheels support standard CPython 3.9 and newer on Linux x86-64 and
AArch64 with glibc 2.28 or newer. A separate wheel supports free-threaded
CPython 3.14 on Linux x86-64 with the same glibc minimum.

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
