"""Compatibility import surface for the NFS-focused VFSI adapter.

The historical :mod:`nfs4fs` package remains fully supported. New code can
import :mod:`vfsi` when it uses the NFS or local dummy backend. SMB users
should install :mod:`vsmbfs`.
"""

from nfs4fs import Nfs4FileSystem, VfsiFileSystem

__all__ = ["Nfs4FileSystem", "VfsiFileSystem"]
