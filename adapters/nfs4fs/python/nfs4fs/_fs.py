"""NFS-specific facade over the shared VFSI fsspec engine."""

import inspect
import operator
import os
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack

from vfsi_fsspec import VfsiFile
from vfsi_fsspec import VfsiFileSystem as _VfsiFileSystem
from vfsi_fsspec._fs import _handle_identity, _RawVfsiFile, _stale_handle_error

from . import _native


class Nfs4FileSystem(_VfsiFileSystem):
    """Vectorized NFSv4 filesystem, with a local dummy backend for testing."""

    protocol = "nfs4"
    _native_module = _native
    _supported_backends = frozenset({"nfs", "dummy"})

    def __init__(self, *args, mount=None, **kwargs):
        """Optionally infer a direct NFS connection from a Linux mounted directory.

        The directory becomes this instance's remote root. Discovery is pinned
        for pooled connections and reconnects; it does not share kernel caches.
        """
        if "_mount_config" in kwargs:
            raise ValueError("_mount_config is internal; use mount= instead")
        self.mount = None
        self.read_only = False
        if mount is not None:
            mount = os.fspath(mount)
            if not os.path.isabs(mount):
                raise ValueError("mount= must be an absolute path")
            if (
                "_mount_config"
                not in inspect.signature(_VfsiFileSystem.__init__).parameters
            ):
                raise ImportError(
                    "mount= requires a vfsi-fsspec build with mount configuration support; upgrade vfsi-fsspec alongside nfs4fs"
                )
            conflicts = {
                "host",
                "root",
                "auth",
                "minor_version",
                "service_principal",
                "dummy_root",
                "backend",
                "share",
                "username",
                "password",
                "domain",
            }
            supplied = sorted(conflicts.intersection(kwargs))
            if args or supplied:
                raise ValueError(
                    "mount= cannot be combined with explicit connection options"
                    + (": " + ", ".join(supplied) if supplied else "")
                )
            config = self._native_module.discover_mount(os.fspath(mount))
            self.mount = os.fspath(config.local_path)
            self.read_only = config.read_only
            kwargs.update(
                host=config.host,
                root="",
                auth="auth_sys",
                minor_version=config.minor_version,
                _mount_config=config,
            )
        super().__init__(*args, **kwargs)

    def read_stream_pipelined(
        self,
        path,
        on_chunk,
        *,
        workers=None,
        chunk_size=1 << 20,
        max_in_flight=8,
        max_buffered_bytes=16 << 20,
    ):
        """Deliver a large file in order while overlapping positional reads.

        The callback receives ``(offset, bytes)``. Its return value is ignored;
        exceptions propagate after all outstanding reads are drained. At most
        ``min(max_in_flight, workers) * chunk_size`` bytes are requested at a
        time, with only one request in flight per worker-owned descriptor.
        This is not a snapshot: concurrent changes can produce mixed data.
        Use for stable files, or check application-level version metadata.
        By default, use up to three sessions from ``connection_pool_size``.
        """
        if self.closed:
            raise ValueError("filesystem is closed")
        if not callable(on_chunk):
            raise TypeError("on_chunk must be callable")
        if workers is None:
            workers = min(3, self.connection_pool_size)
        limits = {}
        for name, value in (
            ("workers", workers),
            ("chunk_size", chunk_size),
            ("max_in_flight", max_in_flight),
            ("max_buffered_bytes", max_buffered_bytes),
        ):
            try:
                if isinstance(value, bool):
                    raise TypeError
                value = operator.index(value)
            except TypeError:
                raise ValueError(f"{name} must be a positive integer") from None
            if value <= 0:
                raise ValueError(f"{name} must be a positive integer")
            limits[name] = value
        workers = limits["workers"]
        chunk_size = limits["chunk_size"]
        max_in_flight = limits["max_in_flight"]
        max_buffered_bytes = limits["max_buffered_bytes"]
        if workers < 1 or workers > self.connection_pool_size:
            raise ValueError("workers must be between 1 and connection_pool_size")
        if not 0 < chunk_size <= min(1 << 20, self.read_all_max_total_bytes):
            raise ValueError("chunk_size must be positive and at most 1 MiB")
        if max_in_flight < 1:
            raise ValueError("max_in_flight must be positive")
        in_flight_limit = min(max_in_flight, workers)
        if in_flight_limit * chunk_size > max_buffered_bytes:
            raise ValueError("read-ahead exceeds max_buffered_bytes")

        internal = self._checked_strip_protocol(path)
        native_path = self._native_path(internal)
        total = 0

        with ExitStack() as stack:
            fds = self._client.open_many_independent(
                [native_path] * workers, ["rb"] * workers
            )
            readers = []
            try:
                for fd in fds:
                    reader = _RawVfsiFile(self, internal, "rb", fd=fd)
                    readers.append(reader)
                    stack.callback(reader.close)
            except BaseException:
                # Close handles not yet transferred to a reader; callbacks
                # already registered on the ExitStack own the earlier ones.
                for fd in fds[len(readers) :]:
                    if self._client.descriptor_valid(fd):
                        try:
                            self._client.close(fd)
                        except BaseException:
                            self._client.defer_close_many([fd])
                raise
            attrs_by_reader = []
            for reader in readers:
                fd = reader._ensure_open()
                try:
                    attrs = self._client.fstat(fd)
                except ConnectionError:
                    if not self.auto_reconnect:
                        raise
                    try:
                        # A dropped reply may leave the original descriptor usable.
                        # Retry metadata on that descriptor before deciding that
                        # its identity cannot be recovered.
                        attrs = self._client.fstat(fd)
                    except ConnectionError:
                        self._client.reconnect_descriptor(fd)
                        # OPEN pinned an object before metadata failed. Since its
                        # identity still could not be captured, reopening by path
                        # could switch to a replacement file.
                        raise _stale_handle_error(internal)
                reader._last_attrs = attrs
                reader._identity = _handle_identity(attrs)
                reader._identity_required = True
                attrs_by_reader.append(attrs)
            size = attrs_by_reader[0]["size"]

            def read_exact(reader, offset, length):
                data = bytearray(length)
                filled = 0
                while filled < length:
                    chunk = reader._pread_at(offset + filled, length - filled)
                    if not chunk:
                        raise OSError(f"short pipelined read at offset {offset}")
                    if len(chunk) > length - filled:
                        raise OSError(f"oversized pipelined read at offset {offset}")
                    data[filled : filled + len(chunk)] = chunk
                    filled += len(chunk)
                return bytes(data)

            pending = deque()
            next_offset = 0
            with ThreadPoolExecutor(max_workers=workers) as executor:
                while next_offset < size or pending:
                    # Consecutive chunks rotate over the readers. A window no
                    # larger than the reader count keeps each mutable descriptor
                    # exclusive to one task, including during reconnect.
                    while next_offset < size and len(pending) < in_flight_limit:
                        offset = next_offset
                        length = min(chunk_size, size - offset)
                        reader = readers[(offset // chunk_size) % workers]
                        pending.append(
                            (
                                offset,
                                executor.submit(read_exact, reader, offset, length),
                            )
                        )
                        next_offset += length
                    offset, future = pending.popleft()
                    data = future.result()
                    on_chunk(offset, data)
                    total += len(data)
        return total


class VfsiFileSystem(Nfs4FileSystem):
    """Compatibility alias for the NFS-focused VFSI distribution."""

    protocol = "vfsi"


Nfs4File = VfsiFile

__all__ = ["Nfs4File", "Nfs4FileSystem", "VfsiFileSystem"]
