"""NFS-specific facade over the shared VFSI fsspec engine."""

from collections import deque
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack

from vfsi_fsspec import VfsiFile
from vfsi_fsspec import VfsiFileSystem as _VfsiFileSystem

from . import _native


class Nfs4FileSystem(_VfsiFileSystem):
    """Vectorized NFSv4 filesystem, with a local dummy backend for testing."""

    protocol = "nfs4"
    _native_module = _native
    _supported_backends = frozenset({"nfs", "dummy"})

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
        ``max_in_flight * chunk_size`` bytes are requested at a time. This is
        not a snapshot: concurrent changes to the file can produce mixed data.
        Use for stable files, or check application-level version metadata.
        By default, use up to three sessions from ``connection_pool_size``.
        """
        if self.closed:
            raise ValueError("filesystem is closed")
        if not callable(on_chunk):
            raise TypeError("on_chunk must be callable")
        if workers is None:
            workers = min(3, self.connection_pool_size)
        if workers < 1 or workers > self.connection_pool_size:
            raise ValueError("workers must be between 1 and connection_pool_size")
        if not 0 < chunk_size <= min(1 << 20, self.read_all_max_total_bytes):
            raise ValueError("chunk_size must be positive and at most 1 MiB")
        if max_in_flight < 1 or max_in_flight * chunk_size > max_buffered_bytes:
            raise ValueError("read-ahead exceeds max_buffered_bytes")

        internal = self._checked_strip_protocol(path)
        native_path = self._native_path(internal)
        total = 0

        with ExitStack() as stack:
            fds = []
            for _ in range(workers):
                fd = self._client.open(native_path, "rb")
                fds.append(fd)
                stack.callback(self._client.close, fd)
            size = self._client.fstat(fds[0])["size"]

            def read_exact(fd, offset, length):
                data = bytearray(length)
                filled = 0
                while filled < length:
                    chunk = self._client.pread(fd, length - filled, offset + filled)
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
                    while next_offset < size and len(pending) < max_in_flight:
                        offset = next_offset
                        length = min(chunk_size, size - offset)
                        fd = fds[(offset // chunk_size) % workers]
                        pending.append(
                            (offset, executor.submit(read_exact, fd, offset, length))
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
