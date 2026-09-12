"""Low-level vectorized SMB2/3 client.

The fsspec adapter is distributed separately as :mod:`vsmbfs`.
"""

from typing import Any

from . import _native

__version__ = _native.__version__
CAP_SERVER_COPY = _native.CAP_SERVER_COPY
CAP_POSIX_METADATA = _native.CAP_POSIX_METADATA
CAP_SYMLINKS = _native.CAP_SYMLINKS
CAP_HARDLINKS = _native.CAP_HARDLINKS
CAP_NON_UTF8_PATHS = _native.CAP_NON_UTF8_PATHS
CAP_LSTAT = _native.CAP_LSTAT
ERR_UNSUPPORTED = _native.ERR_UNSUPPORTED


class SmbClient:
    """A native SMB client exposing scalar and vector filesystem operations."""

    def __init__(
        self,
        host: str,
        share: str,
        username: str = "",
        password: str = "",
        domain: str = "",
        connect_timeout: float = 10.0,
        request_timeout: float = 5.0,
    ) -> None:
        if not share:
            raise ValueError("share must not be empty")
        self._client = _native.NfsClient(
            host=host,
            backend="smb",
            share=share,
            username=username,
            password=password,
            domain=domain,
            connect_timeout=connect_timeout,
            request_timeout=request_timeout,
        )

    def __getattr__(self, name: str) -> Any:
        return getattr(self._client, name)

    def close(self) -> None:
        """Close the underlying network client."""

        self._client.shutdown()

    def shutdown(self) -> None:
        """Alias for :meth:`close`, matching the native client."""

        self.close()

    def __enter__(self) -> "SmbClient":
        return self

    def __exit__(self, *args: object) -> None:
        self.close()


Client = SmbClient

__all__ = [
    "CAP_HARDLINKS",
    "CAP_LSTAT",
    "CAP_NON_UTF8_PATHS",
    "CAP_POSIX_METADATA",
    "CAP_SERVER_COPY",
    "CAP_SYMLINKS",
    "Client",
    "ERR_UNSUPPORTED",
    "SmbClient",
]
