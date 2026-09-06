"""fsspec filesystems backed by the vectorized VFSI clients.

Importing this package registers the compatible ``nfs4`` protocol and the
protocol-neutral ``vfsi`` alias with fsspec:

    import fsspec
    import nfs4fs

    fs = fsspec.filesystem("nfs4", host="127.0.0.1", root="git/some/tree")
"""

import fsspec

from . import _native
from ._fs import Nfs4File, Nfs4FileSystem, VfsiFileSystem

__all__ = ["Nfs4FileSystem", "VfsiFileSystem", "Nfs4File"]
__version__ = _native.__version__

fsspec.register_implementation("nfs4", Nfs4FileSystem)
fsspec.register_implementation("vfsi", VfsiFileSystem)
