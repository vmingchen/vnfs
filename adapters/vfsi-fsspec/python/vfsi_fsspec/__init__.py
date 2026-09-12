"""Backend-neutral fsspec implementation shared by VFSI protocol packages."""

from ._blockcache import install_fsspec_blockcache_compat
from ._fs import VfsiFile, VfsiFileSystem

__all__ = ["VfsiFile", "VfsiFileSystem", "install_fsspec_blockcache_compat"]
