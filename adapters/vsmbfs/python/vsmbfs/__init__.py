"""fsspec filesystem backed by the vectorized :mod:`vsmb` client."""

from importlib.metadata import version

import fsspec
from vfsi_fsspec import install_fsspec_blockcache_compat

from ._fs import SmbFileSystem, VsmbFile, VsmbFileSystem

__all__ = ["SmbFileSystem", "VsmbFile", "VsmbFileSystem"]
__version__ = version("vsmbfs")

fsspec.register_implementation("vsmbfs", VsmbFileSystem)
install_fsspec_blockcache_compat(VsmbFileSystem)
