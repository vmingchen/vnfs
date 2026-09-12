"""fsspec filesystem backed by the vectorized :mod:`vsmb` client."""

import fsspec
from vfsi_fsspec import install_fsspec_blockcache_compat
from vsmb import __version__ as __version__

from ._fs import SmbFileSystem, VsmbFile, VsmbFileSystem

__all__ = ["SmbFileSystem", "VsmbFile", "VsmbFileSystem"]

fsspec.register_implementation("vsmbfs", VsmbFileSystem)
install_fsspec_blockcache_compat(VsmbFileSystem)
