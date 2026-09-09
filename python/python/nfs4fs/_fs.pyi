"""Public typing surface for the VFSI fsspec adapter."""

import datetime
import io
from os import PathLike
from typing import Any, BinaryIO, Callable, Iterator, Mapping, Optional, Sequence, Union

from fsspec.spec import AbstractFileSystem

Path = Union[str, PathLike[str]]
BytesLike = Union[bytes, bytearray, memoryview]

class Nfs4File(io.RawIOBase):
    fs: Nfs4FileSystem
    path: str
    mode: str
    def __init__(
        self,
        fs: Nfs4FileSystem,
        path: Path,
        mode: str = "rb",
        fd: Optional[int] = None,
    ) -> None: ...
    @property
    def closed(self) -> bool: ...
    @property
    def size(self) -> int: ...
    def readable(self) -> bool: ...
    def writable(self) -> bool: ...
    def seekable(self) -> bool: ...
    def readinto(self, b: Any) -> int: ...
    def read(self, size: int = -1) -> bytes: ...
    def write(self, data: BytesLike) -> int: ...
    def seek(self, offset: int, whence: int = 0) -> int: ...
    def tell(self) -> int: ...
    def truncate(self, size: Optional[int] = None) -> int: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class Nfs4FileSystem(AbstractFileSystem):
    protocol: str
    root_marker: str
    host: str
    backend: str
    auto_mkdir: bool
    compound_size_limit: Optional[int]
    minor_version: Optional[int]
    share: Optional[str]
    username: str
    domain: str
    batch_size: int
    max_batch_bytes: int
    transfer_chunk_size: int
    transaction_spool_threshold: int
    connect_timeout: float
    request_timeout: float
    auto_reconnect: bool
    def __init__(
        self,
        host: str = "127.0.0.1",
        root: str = "",
        backend: str = "nfs",
        dummy_root: Optional[Path] = None,
        auto_mkdir: bool = False,
        compound_size_limit: Optional[int] = None,
        minor_version: Optional[int] = None,
        share: Optional[str] = None,
        username: str = "",
        password: str = "",
        domain: str = "",
        batch_size: int = 128,
        max_batch_bytes: int = 67_108_864,
        transfer_chunk_size: int = 8_388_608,
        transaction_spool_threshold: int = 8_388_608,
        connect_timeout: float = 10.0,
        request_timeout: float = 5.0,
        auto_reconnect: bool = True,
        **kwargs: Any,
    ) -> None: ...
    @property
    def closed(self) -> bool: ...
    def close(self) -> None: ...
    def __enter__(self) -> Nfs4FileSystem: ...
    def __exit__(self, exc_type: Any, exc_value: Any, traceback: Any) -> None: ...
    def smb_dialect(self) -> Optional[int]: ...
    def info(self, path: Path, **kwargs: Any) -> dict[str, Any]: ...
    def ls(
        self, path: Path, detail: bool = True, **kwargs: Any
    ) -> Union[list[str], list[dict[str, Any]]]: ...
    def exists(self, path: Path, **kwargs: Any) -> bool: ...
    def isfile(self, path: Path) -> bool: ...
    def isdir(self, path: Path) -> bool: ...
    def size(self, path: Path) -> int: ...
    def sizes(self, paths: Sequence[Path]) -> list[int]: ...
    def created(self, path: Path) -> Optional[datetime.datetime]: ...
    def modified(self, path: Path) -> Optional[datetime.datetime]: ...
    def ukey(self, path: Path) -> str: ...
    def checksum(self, path: Path) -> int: ...
    def cat(
        self,
        path: Union[Path, Sequence[Path]],
        recursive: bool = False,
        on_error: str = "raise",
        **kwargs: Any,
    ) -> Union[bytes, dict[str, Union[bytes, BaseException]]]: ...
    def cat_file(
        self,
        path: Path,
        start: Optional[int] = None,
        end: Optional[int] = None,
        **kwargs: Any,
    ) -> bytes: ...
    def cat_ranges(
        self,
        paths: Sequence[Path],
        starts: Union[int, Sequence[int]],
        ends: Union[Optional[int], Sequence[Optional[int]]],
        max_gap: Optional[int] = None,
        on_error: str = "return",
        **kwargs: Any,
    ) -> list[Union[bytes, BaseException]]: ...
    def pipe(
        self,
        path: Union[Path, Mapping[Path, BytesLike]],
        value: Optional[BytesLike] = None,
        **kwargs: Any,
    ) -> None: ...
    def pipe_file(
        self,
        path: Path,
        value: BytesLike,
        mode: str = "overwrite",
        **kwargs: Any,
    ) -> None: ...
    def get(
        self,
        rpath: Union[Path, Sequence[Path]],
        lpath: Union[Path, Sequence[Path]],
        recursive: bool = False,
        callback: Any = ...,
        maxdepth: Optional[int] = None,
        **kwargs: Any,
    ) -> None: ...
    def put(
        self,
        lpath: Union[Path, Sequence[Path]],
        rpath: Union[Path, Sequence[Path]],
        recursive: bool = False,
        callback: Any = ...,
        maxdepth: Optional[int] = None,
        **kwargs: Any,
    ) -> None: ...
    def _open(
        self,
        path: Path,
        mode: str = "rb",
        block_size: Optional[int] = None,
        autocommit: bool = True,
        cache_options: Optional[dict[str, Any]] = None,
        **kwargs: Any,
    ) -> Union[Nfs4File, BinaryIO]: ...
    def open_many(self, open_files: Sequence[Any]) -> list[Nfs4File]: ...
    def commit_many(self, open_files: Sequence[Nfs4File]) -> None: ...
    def mkdir(self, path: Path, create_parents: bool = True, **kwargs: Any) -> None: ...
    def makedirs(self, path: Path, exist_ok: bool = False) -> None: ...
    def rmdir(self, path: Path) -> None: ...
    def rm(
        self,
        path: Union[Path, Sequence[Path]],
        recursive: bool = False,
        maxdepth: Optional[int] = None,
    ) -> None: ...
    def mv(
        self,
        path1: Path,
        path2: Path,
        recursive: bool = False,
        maxdepth: Optional[int] = None,
        **kwargs: Any,
    ) -> None: ...
    def cp_file(self, path1: Path, path2: Path, **kwargs: Any) -> None: ...
    def copy(
        self,
        path1: Union[Path, Sequence[Path]],
        path2: Union[Path, Sequence[Path]],
        recursive: bool = False,
        maxdepth: Optional[int] = None,
        on_error: Optional[str] = None,
        **kwargs: Any,
    ) -> None: ...
    def cp(self, *args: Any, **kwargs: Any) -> None: ...
    def touch(self, path: Path, truncate: bool = True, **kwargs: Any) -> None: ...
    def symlink(self, target: Path, path: Path, **kwargs: Any) -> None: ...
    def readlink(self, path: Path) -> str: ...
    def hardlink(self, src: Path, dst: Path) -> None: ...
    def walk(
        self,
        path: Path,
        maxdepth: Optional[int] = None,
        topdown: bool = True,
        on_error: Union[str, Callable[[OSError], Any]] = "omit",
        **kwargs: Any,
    ) -> Iterator[Any]: ...
    def find(
        self,
        path: Path,
        maxdepth: Optional[int] = None,
        withdirs: bool = False,
        detail: bool = False,
        **kwargs: Any,
    ) -> Union[list[str], dict[str, dict[str, Any]]]: ...
    def du(
        self,
        path: Path,
        total: bool = True,
        maxdepth: Optional[int] = None,
        withdirs: bool = False,
        **kwargs: Any,
    ) -> Union[int, dict[str, int]]: ...

class VfsiFileSystem(Nfs4FileSystem):
    protocol: str
