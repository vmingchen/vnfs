from os import PathLike
from typing import Any, Callable, Optional, Union

from vfsi_fsspec import VfsiFile
from vfsi_fsspec import VfsiFileSystem as _VfsiFileSystem

class Nfs4FileSystem(_VfsiFileSystem):
    protocol: str
    mount: Optional[str]
    read_only: bool
    def __init__(
        self,
        host: str = "127.0.0.1",
        root: str = "",
        backend: str = "nfs",
        dummy_root: Optional[Union[str, PathLike[str]]] = None,
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
        block_size: int = 1_048_576,
        cache_type: Optional[str] = "readahead",
        cache_options: Optional[dict[str, Any]] = None,
        write_buffering: bool = False,
        vectorized_buffering: bool = True,
        connect_timeout: float = 10.0,
        request_timeout: float = 5.0,
        auto_reconnect: bool = True,
        use_listings_cache: bool = False,
        listings_expiry_time: Optional[float] = None,
        max_paths: Optional[int] = None,
        read_all_max_total_bytes: int = 16_777_216,
        directory_max_entries: int = 100_000,
        directory_max_path_bytes: int = 16_777_216,
        walk_max_depth: int = 128,
        auth: Optional[str] = None,
        service_principal: Optional[str] = None,
        connection_pool_size: int = 1,
        mount: Optional[Union[str, PathLike[str]]] = None,
        **kwargs: Any,
    ) -> None: ...
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
