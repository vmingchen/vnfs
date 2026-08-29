"""fsspec ``nfs4://`` filesystem backed by the vectorized vnfs NFSv4.1 client.

Importing this package registers the ``nfs4`` protocol with fsspec:

    import fsspec
    import vnfs_fs

    fs = fsspec.filesystem("nfs4", host="127.0.0.1", root="git/some/tree")
"""

import fsspec

from . import _native
from ._fs import Nfs4File, Nfs4FileSystem

__all__ = ["Nfs4FileSystem", "Nfs4File"]
__version__ = _native.__version__

fsspec.register_implementation("nfs4", Nfs4FileSystem)
