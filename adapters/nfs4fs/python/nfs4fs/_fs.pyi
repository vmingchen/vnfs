from vfsi_fsspec import VfsiFile
from vfsi_fsspec import VfsiFileSystem as _VfsiFileSystem

class Nfs4FileSystem(_VfsiFileSystem):
    protocol: str

class VfsiFileSystem(Nfs4FileSystem):
    protocol: str

Nfs4File = VfsiFile

__all__: list[str]
