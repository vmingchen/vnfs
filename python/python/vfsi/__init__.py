"""Protocol-neutral import surface for the VFSI fsspec adapter.

The historical :mod:`nfs4fs` package remains fully supported. New code can
import :mod:`vfsi` when it may select NFS, SMB, or the local dummy backend.
"""

from nfs4fs import Nfs4FileSystem, VfsiFileSystem

__all__ = ["Nfs4FileSystem", "VfsiFileSystem"]
