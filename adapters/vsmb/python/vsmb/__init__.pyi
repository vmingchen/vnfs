from . import _native as _native

__version__: str
CAP_SERVER_COPY: int
CAP_POSIX_METADATA: int
CAP_SYMLINKS: int
CAP_HARDLINKS: int
CAP_NON_UTF8_PATHS: int
CAP_LSTAT: int
ERR_UNSUPPORTED: int

class SmbClient(_native.NfsClient):
    def __init__(
        self,
        host: str,
        share: str,
        username: str = "",
        password: str = "",
        domain: str = "",
        connect_timeout: float = 10.0,
        request_timeout: float = 5.0,
    ) -> None: ...
    def close(self) -> None: ...
    def shutdown(self) -> None: ...
    def __enter__(self) -> SmbClient: ...
    def __exit__(self, *args: object) -> None: ...

Client = SmbClient
__all__: list[str]
