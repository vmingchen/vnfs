from os import PathLike
from typing import Callable, Optional, Union

from vfsi_fsspec import VfsiFile
from vfsi_fsspec import VfsiFileSystem as _VfsiFileSystem

class Nfs4FileSystem(_VfsiFileSystem):
    protocol: str
    def read_stream_pipelined(
        self,
        path: Union[str, PathLike[str]],
        on_chunk: Callable[[int, bytes], object],
        *,
        workers: Optional[int] = None,
        chunk_size: int = 1_048_576,
        max_in_flight: int = 8,
        max_buffered_bytes: int = 16_777_216,
    ) -> int: ...

class VfsiFileSystem(Nfs4FileSystem):
    protocol: str

Nfs4File = VfsiFile

__all__: list[str]
