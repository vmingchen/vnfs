from typing import Any, Optional

from vfsi_fsspec import VfsiFile, VfsiFileSystem

VsmbFile = VfsiFile

class VsmbFileSystem(VfsiFileSystem):
    protocol: str
    def __init__(
        self,
        host: str = "127.0.0.1",
        root: str = "",
        share: Optional[str] = None,
        **kwargs: Any,
    ) -> None: ...

SmbFileSystem = VsmbFileSystem
