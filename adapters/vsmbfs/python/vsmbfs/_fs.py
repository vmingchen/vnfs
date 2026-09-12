"""SMB-specific facade over the shared VFSI fsspec engine."""

from vfsi_fsspec import VfsiFile
from vfsi_fsspec import VfsiFileSystem as _VfsiFileSystem
from vsmb import _native


class VsmbFileSystem(_VfsiFileSystem):
    """Vectorized SMB2/3 filesystem."""

    protocol = "vsmbfs"
    _native_module = _native
    _supported_backends = frozenset({"smb"})

    def __init__(self, host="127.0.0.1", root="", share=None, **kwargs):
        backend = kwargs.pop("backend", "smb")
        if backend != "smb":
            raise ValueError("vsmbfs only supports backend='smb'")
        super().__init__(
            host=host,
            root=root,
            backend="smb",
            share=share,
            **kwargs,
        )


SmbFileSystem = VsmbFileSystem
VsmbFile = VfsiFile

__all__ = ["SmbFileSystem", "VsmbFile", "VsmbFileSystem"]
