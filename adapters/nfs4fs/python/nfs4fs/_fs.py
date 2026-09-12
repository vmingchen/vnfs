"""NFS-specific facade over the shared VFSI fsspec engine."""

from vfsi_fsspec import VfsiFile
from vfsi_fsspec import VfsiFileSystem as _VfsiFileSystem

from . import _native


class Nfs4FileSystem(_VfsiFileSystem):
    """Vectorized NFSv4 filesystem, with a local dummy backend for testing."""

    protocol = "nfs4"
    _native_module = _native
    _supported_backends = frozenset({"nfs", "dummy"})


class VfsiFileSystem(Nfs4FileSystem):
    """Compatibility alias for the NFS-focused VFSI distribution."""

    protocol = "vfsi"


Nfs4File = VfsiFile

__all__ = ["Nfs4File", "Nfs4FileSystem", "VfsiFileSystem"]
