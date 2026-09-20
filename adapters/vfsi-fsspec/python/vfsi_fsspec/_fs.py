"""fsspec filesystem implementation for the vectorized VFSI backends."""

import datetime
import errno
import hashlib
import io
import math
import os
import posixpath
import shutil
import tempfile
import threading
import uuid
import weakref
from contextlib import ExitStack
from glob import has_magic
from urllib.parse import unquote, urlsplit

from fsspec.caching import caches
from fsspec.callbacks import DEFAULT_CALLBACK, Callback
from fsspec.spec import AbstractBufferedFile, AbstractFileSystem
from fsspec.transaction import Transaction

__all__ = ["VfsiFile", "VfsiFileSystem"]

_MEMORY_PATH = "<memory>"
_ERR_SAME_FILE = 0xFFFF_FFFD
_ERR_UNSUPPORTED = 0xFFFF_FFFE


def _normalize_mode(mode):
    """Collapse 'b'/'t' characters: 'rt'/'r'/'rb' -> 'r', 'wb+' -> 'w+', ..."""
    base = mode.replace("b", "").replace("t", "")
    if base not in ("r", "r+", "w", "w+", "a", "a+", "x", "x+"):
        raise ValueError(f"unsupported mode: {mode!r}")
    return base


def _oserror(errno_code, path):
    """Rebuild a Python exception from a native errno (for batched results)."""
    if errno_code == _ERR_UNSUPPORTED:
        return NotImplementedError(f"operation is unsupported: {path!r}")
    # Constructing OSError with an errno lets Python select the appropriate
    # subclass (FileNotFoundError, PermissionError, ...), and passing the path
    # separately preserves the useful ``exception.filename`` attribute.
    try:
        message = os.strerror(errno_code)
    except (OverflowError, ValueError):
        message = f"errno {errno_code}"
    return OSError(errno_code, message, path)


def _info_dict(full_name, attrs):
    """Build an fsspec `info` dict from a native attribute dict."""
    info = {
        "name": full_name,
        "type": attrs["type"],
        "size": attrs.get("size"),
        "mode": attrs.get("mode"),
        "uid": attrs.get("uid"),
        "gid": attrs.get("gid"),
        "nlink": attrs.get("nlink"),
        "fileid": attrs.get("fileid"),
        "change": attrs.get("change"),
        "created": attrs.get("created"),
        "created_ns": attrs.get("created_ns"),
        "modified": attrs.get("modified"),
        "modified_ns": attrs.get("modified_ns"),
        "accessed": attrs.get("accessed"),
        "accessed_ns": attrs.get("accessed_ns"),
        "checksum": attrs.get("checksum"),
        "islink": attrs.get("islink", False),
    }
    # POSIX identity fields remain part of the stable fsspec metadata shape
    # even when an NFSv4 principal cannot be mapped safely to a local NSS id.
    # `None` is more accurate than dropping the keys or stripping an arbitrary
    # remote identity domain and potentially reporting the wrong local user.
    return {k: v for k, v in info.items() if v is not None or k in {"uid", "gid"}}


def _depth(path):
    """Number of components below the root for an internal path."""
    if path == "/":
        return 0
    return path.count("/")


def _normalize_range(start, end, size=None):
    """Normalize fsspec byte-range bounds and reject negative lengths."""
    start = 0 if start is None else start
    if start < 0:
        if size is None:
            raise ValueError("size is required for a negative start")
        start = max(0, size + start)
    if end is not None and end < 0:
        if size is None:
            raise ValueError("size is required for a negative end")
        end = size + end
    if end is not None and end == start - 1:
        # LocalFileSystem passes this through to read(-1), which means read to
        # EOF. Preserve that established fsspec behavior for differential
        # compatibility even though it differs from ordinary slice notation.
        end = None
    elif end is not None and end < start:
        raise ValueError("read length must be non-negative or -1")
    return start, end


def _bounded_batches(sizes, max_items, max_bytes):
    """Yield index lists bounded by both item count and aggregate bytes."""
    batch = []
    batch_bytes = 0
    for index, size in enumerate(sizes):
        size = max(0, int(size or 0))
        if batch and (len(batch) >= max_items or batch_bytes + size > max_bytes):
            yield batch
            batch = []
            batch_bytes = 0
        batch.append(index)
        batch_bytes += size
    if batch:
        yield batch


def _complete_callback(callback, size):
    """Mark one byte-oriented callback complete, including empty transfers."""
    callback.set_size(size)
    callback.relative_update(size)


def _complete_child(callback, path1, path2, size):
    """Report one completed item and its byte count to a bulk callback."""
    with callback.branched(path1, path2) as child:
        _complete_callback(child, size)
    callback.relative_update()


def _allocation_error(path, requested, limit):
    """Return the uniform error used when a bytes-returning API exceeds its budget."""
    return OSError(
        errno.EFBIG,
        f"read would allocate {requested} bytes, exceeding the {limit}-byte limit",
        path,
    )


class _ResilientClient:
    """Own a native session with fork detection and safe read reconnects."""

    _IDEMPOTENT = {
        "minor_version",
        "smb_dialect",
        "capabilities",
        "server_copy_enabled",
        "stat",
        "lstat",
        "exists",
        "stat_many",
        "lstat_many",
        "exists_many",
        "read_many",
        "read_all_many",
        "listdir",
        "listdir_many",
        "walk",
        "readlink",
        "getcwd",
    }

    def __init__(self, native_module, factory_args, auto_reconnect=True):
        self._native_module = native_module
        self._factory_args = factory_args
        self._auto_reconnect = auto_reconnect
        self._native = native_module.NfsClient(*factory_args)
        self._pid = os.getpid()
        self._generation = 0

    @property
    def generation(self):
        return self._generation

    @property
    def closed(self):
        return self._native is None

    def ensure_ready(self):
        if self.closed:
            raise ValueError("filesystem is closed")
        if self._pid != os.getpid():
            self.reconnect(after_fork=True)

    def reconnect(self, after_fork=False):
        old = self._native
        self._native = self._native_module.NfsClient(*self._factory_args)
        self._pid = os.getpid()
        self._generation += 1
        if old is not None:
            if after_fork:
                # Sending CLOSE/DESTROY_SESSION over the inherited connection
                # would corrupt the parent's live protocol state.
                old._abandon_after_fork()
            else:
                old.shutdown()
        del old

    def shutdown(self):
        old = self._native
        self._native = None
        self._generation += 1
        if old is not None:
            old.shutdown()
        del old

    def __getattr__(self, name):
        self.ensure_ready()
        target = getattr(self._native, name)
        if not callable(target):
            return target

        def call(*args, **kwargs):
            self.ensure_ready()
            try:
                return getattr(self._native, name)(*args, **kwargs)
            except ConnectionError:
                if not self._auto_reconnect or name not in self._IDEMPOTENT:
                    raise
                self.reconnect()
                return getattr(self._native, name)(*args, **kwargs)

        return call


class _ClientPool:
    """Bounded pool of independent native sessions with virtual descriptors.

    Path operations are distributed across sessions, while a virtual file
    descriptor is always routed back to the session that opened it. This
    avoids the native client's per-session mutex becoming a filesystem-wide
    serialization point without sacrificing vector operations within a batch.
    """

    _FD_METHODS = frozenset(
        {
            "close",
            "read",
            "write",
            "write_positioned",
            "pread",
            "pwrite",
            "fseek",
            "fstat",
        }
    )
    _FD_MANY_METHODS = frozenset(
        {"fstat_many", "pread_many", "pwrite_many", "append_many", "close_many"}
    )

    def __init__(self, native_module, factory_args, size=1, auto_reconnect=True):
        self._clients = [
            _ResilientClient(native_module, factory_args, auto_reconnect=False)
            for _ in range(size)
        ]
        self._auto_reconnect = auto_reconnect
        self._lock = threading.RLock()
        # This lock must remain stable while reconnect replaces the per-client
        # locks inherited from a multithreaded parent after fork.
        self._reconnect_lock = threading.RLock()
        self._client_locks = [threading.RLock() for _ in self._clients]
        self._next_client = 0
        # Keep ordinary invalid descriptors such as -1 invalid at the public
        # seam. Virtual handles occupy a remote, monotonically decreasing range.
        self._next_fd = -(1 << 62)
        self._fds = {}
        self._deferred_close = set()
        self._generation = 0
        self._pid = os.getpid()
        self._cwd = "/"
        if hasattr(os, "register_at_fork"):
            pool_ref = weakref.ref(self)

            def reset_pool_locks():
                pool = pool_ref()
                if pool is not None:
                    pool._reset_locks_after_fork()

            os.register_at_fork(after_in_child=reset_pool_locks)

    def _reset_locks_after_fork(self):
        """Discard locks that may be owned by threads absent in the child."""
        self._lock = threading.RLock()
        self._reconnect_lock = threading.RLock()
        self._client_locks = [threading.RLock() for _ in self._clients]

    @property
    def generation(self):
        return self._generation

    @property
    def closed(self):
        return not self._clients

    def _choose(self):
        with self._lock:
            if not self._clients:
                raise ValueError("filesystem is closed")
            index = self._next_client % len(self._clients)
            self._next_client += 1
            return index, self._clients[index]

    def _register(self, owner, native_fd):
        with self._lock:
            token = self._next_fd
            self._next_fd -= 1
            self._fds[token] = (owner, native_fd)
            return token

    def _resolve(self, token):
        with self._lock:
            try:
                owner, native_fd = self._fds[token]
                return owner, native_fd, self._clients[owner]
            except (KeyError, IndexError):
                raise OSError(errno.EBADF, "Bad file descriptor") from None

    def descriptor_valid(self, token):
        """Return whether a virtual descriptor still belongs to a live session."""
        with self._lock:
            return token in self._fds

    def defer_close_many(self, tokens):
        """Retain cleanup ownership when setup fails and CLOSE is unavailable."""
        with self._lock:
            self._deferred_close.update(token for token in tokens if token in self._fds)

    def _discard_tokens(self, tokens):
        with self._lock:
            for token in tokens:
                self._fds.pop(token, None)
                self._deferred_close.discard(token)

    def _retry_deferred_closes(self):
        """Best-effort retry descriptors orphaned by a failed setup cleanup."""
        with self._lock:
            by_owner = {}
            for token in tuple(self._deferred_close):
                mapping = self._fds.get(token)
                if mapping is not None:
                    by_owner.setdefault(mapping[0], []).append(token)
                else:
                    self._deferred_close.discard(token)
        for owner, tokens in by_owner.items():
            with self._client_locks[owner]:
                with self._lock:
                    live = [
                        (token, self._fds[token][1])
                        for token in tokens
                        if token in self._fds and self._fds[token][0] == owner
                    ]
                    client = self._clients[owner]
                if not live:
                    continue
                try:
                    client.close_many([native_fd for _, native_fd in live])
                except ConnectionError:
                    if self._auto_reconnect:
                        try:
                            self._reconnect_client(owner)
                        except BaseException:
                            pass
                except BaseException as error:
                    completed = getattr(error, "index", 0)
                    if isinstance(completed, int) and 0 <= completed <= len(live):
                        self._discard_tokens([token for token, _ in live[:completed]])
                else:
                    self._discard_tokens([token for token, _ in live])

    def ensure_ready(self):
        if self.closed:
            raise ValueError("filesystem is closed")
        current_pid = os.getpid()
        if self._pid != current_pid:
            with self._reconnect_lock:
                # Another child thread may have completed recovery while this
                # thread waited. Only one thread may rebuild inherited sessions.
                if self._pid != current_pid:
                    self._reconnect_all(after_fork=True)

    def _reconnect_client(self, owner, after_fork=False):
        """Replace one failed session without disrupting healthy pool members."""
        with self._client_locks[owner]:
            with self._lock:
                if owner >= len(self._clients):
                    raise ValueError("filesystem is closed")
                client = self._clients[owner]
                cwd = self._cwd
                previous_generation = client.generation
            try:
                client.reconnect(after_fork=after_fork)
                if cwd != "/":
                    client.chdir(cwd)
            finally:
                # reconnect() installs the replacement before retiring the old
                # session. Even if retirement fails, old descriptors can never
                # be sent to the replacement connection.
                if client.generation != previous_generation:
                    with self._lock:
                        stale = [
                            token
                            for token, (token_owner, _) in self._fds.items()
                            if token_owner == owner
                        ]
                        self._discard_tokens(stale)
                        self._generation += 1

    def reconnect_descriptor(self, token):
        """Reconnect only the session that owns token."""
        owner, _, _ = self._resolve(token)
        self._reconnect_client(owner)

    def _reconnect_all(self, after_fork=False):
        with self._lock:
            owners = list(range(len(self._clients)))
            if after_fork:
                # Locks may have been held by threads that did not survive fork.
                self._client_locks = [threading.RLock() for _ in self._clients]
        error = None
        for owner in owners:
            try:
                self._reconnect_client(owner, after_fork=after_fork)
            except BaseException as exc:
                error = error or exc
        if error is not None:
            raise error
        self._pid = os.getpid()

    def reconnect(self, after_fork=False):
        with self._reconnect_lock:
            self._reconnect_all(after_fork=after_fork)

    def shutdown(self):
        with self._lock:
            clients, self._clients = self._clients, []
            self._fds.clear()
            self._deferred_close.clear()
            self._generation += 1
        error = None
        for owner, client in enumerate(clients):
            try:
                with self._client_locks[owner]:
                    client.shutdown()
            except BaseException as exc:
                error = error or exc
        if error is not None:
            raise error

    def open(self, path, mode):
        self.ensure_ready()
        self._retry_deferred_closes()
        owner, client = self._choose()
        with self._client_locks[owner]:
            return self._register(owner, client.open(path, mode))

    def open_many(self, paths, modes):
        self.ensure_ready()
        self._retry_deferred_closes()
        owner, client = self._choose()
        with self._client_locks[owner]:
            return [self._register(owner, fd) for fd in client.open_many(paths, modes)]

    def chdir(self, path):
        """Keep the session-local working directory identical across the pool."""
        self.ensure_ready()
        try:
            with ExitStack() as stack:
                for lock in self._client_locks:
                    stack.enter_context(lock)
                for client in self._clients:
                    client.chdir(path)
                self._cwd = path
        except BaseException:
            # Rebuild every session rather than leave a partially changed pool.
            self._cwd = "/"
            self.reconnect()
            raise

    def getcwd(self):
        self.ensure_ready()
        with self._client_locks[0]:
            return self._clients[0].getcwd()

    def __getattr__(self, name):
        if name in self._FD_METHODS:

            def fd_call(token, *args, **kwargs):
                self.ensure_ready()
                owner, _, _ = self._resolve(token)
                with self._client_locks[owner]:
                    _, native_fd, client = self._resolve(token)
                    result = getattr(client, name)(native_fd, *args, **kwargs)
                if name == "close":
                    self._discard_tokens([token])
                return result

            return fd_call

        if name in self._FD_MANY_METHODS:

            def fd_many_call(tokens, *args, **kwargs):
                self.ensure_ready()
                resolved = []
                for index, token in enumerate(tokens):
                    try:
                        resolved.append(self._resolve(token))
                    except OSError as error:
                        error.index = index
                        raise
                owners = {owner for owner, _, _ in resolved}
                if len(owners) > 1:
                    raise ValueError("descriptor vector spans multiple native sessions")
                if not resolved:
                    owner, client = self._choose()
                    native_fds = []
                else:
                    owner = resolved[0][0]
                with self._client_locks[owner]:
                    if resolved:
                        resolved = [self._resolve(token) for token in tokens]
                        client = resolved[0][2]
                        native_fds = [native_fd for _, native_fd, _ in resolved]
                    try:
                        result = getattr(client, name)(native_fds, *args, **kwargs)
                    except BaseException as error:
                        if name == "close_many":
                            completed = getattr(error, "index", 0)
                            if isinstance(completed, int) and 0 <= completed <= len(
                                tokens
                            ):
                                self._discard_tokens(tokens[:completed])
                        raise
                if name == "close_many":
                    self._discard_tokens(tokens)
                return result

            return fd_many_call

        def path_call(*args, **kwargs):
            self.ensure_ready()
            self._retry_deferred_closes()
            owner, client = self._choose()
            with self._client_locks[owner]:
                try:
                    return getattr(client, name)(*args, **kwargs)
                except ConnectionError:
                    if (
                        not self._auto_reconnect
                        or name not in _ResilientClient._IDEMPOTENT
                    ):
                        raise
                    self._reconnect_client(owner)
                    return getattr(client, name)(*args, **kwargs)

        return path_call


class _RawVfsiFile(io.RawIOBase):
    """A binary file object backed by an open vnfs descriptor.

    The descriptor is opened lazily (first I/O) for read modes so bulk paths
    (``cat``/``cat_ranges``/``OpenFiles`` reads) never pay per-file OPEN/CLOSE
    compounds; write/append modes open eagerly so ``wb`` truncates and ``ab``
    appends at open time, matching POSIX ``open()``.
    """

    def __init__(self, fs, path, mode="rb", fd=None):
        super().__init__()
        # Set these first so close()/__del__ are safe even if later
        # initialization (e.g. an unsupported mode) raises.
        self._closed = False
        self._fd = fd  # None until the first I/O (read modes) or __init__ (w/a)
        self._fd_generation = fs._client.generation if fd is not None else None
        self._broken = False
        self.fs = fs
        self.path = fs._strip_protocol(path)
        self.mode = mode
        self._base_mode = _normalize_mode(mode)
        self._pos = 0
        self._cached_size = None
        self._readable = self._base_mode in ("r", "r+", "w+", "a+", "x+")
        self._writable = self._base_mode in ("r+", "w", "w+", "a", "a+", "x", "x+")
        if fd is None and self._base_mode in ("w", "w+", "a", "a+", "x", "x+"):
            # These modes can create a directory entry, and ``w`` can also
            # truncate it. Invalidate before the native mutation so ambiguous
            # failures cannot leave stale contents or directory metadata.
            self.fs._invalidate_namespace([self.path])
        if fd is None and self._base_mode in ("w", "w+", "a", "a+", "x", "x+"):
            self._ensure_open()

    # -- internals ---------------------------------------------------------

    def _native_mode(self):
        return {
            "r": "rb",
            "r+": "rb+",
            "w": "wb",
            "w+": "wb+",
            "a": "ab",
            "a+": "ab+",
            "x": "xb",
            "x+": "xb+",
        }[self._base_mode]

    def _ensure_open(self):
        if self._broken:
            raise ConnectionError("write handle is unusable after a transport failure")
        self.fs._client.ensure_ready()
        if self._fd is not None and not self.fs._client.descriptor_valid(self._fd):
            self._fd = None
            self._fd_generation = None
            if self._writable:
                self._broken = True
                raise ConnectionError(
                    "write handle is unusable after its native session reconnects"
                )
        if self._fd is None:
            self._fd = self.fs._client.open(
                self.fs._native_path(self.path), self._native_mode()
            )
            self._fd_generation = self.fs._client.generation
            if self._base_mode.startswith("a"):
                attrs = self.fs._client.fstat(self._fd)
                self._pos = attrs["size"]
                self._cached_size = attrs["size"]
        return self._fd

    def _pread_at(self, offset, length):
        """Retry an absolute-offset read once on a fresh session."""
        fd = self._ensure_open()
        try:
            return self.fs._client.pread(fd, length, offset)
        except (ConnectionError, OSError) as error:
            stale = not self.fs._client.descriptor_valid(fd)
            if isinstance(error, OSError) and not isinstance(error, ConnectionError):
                if error.errno != errno.EBADF or not stale:
                    raise
            if not self.fs.auto_reconnect:
                raise
            if not stale:
                self.fs._client.reconnect_descriptor(fd)
            self._fd = None
            self._fd_generation = None
            fd = self._ensure_open()
            return self.fs._client.pread(fd, length, offset)

    def _pread(self, length):
        return self._pread_at(self._pos, length)

    def _size(self):
        if self._cached_size is None:
            if self._fd is not None:
                self._cached_size = self.fs._client.fstat(self._ensure_open())["size"]
            else:
                self._cached_size = self.fs.size(self.path)
        return self._cached_size

    # -- io.RawIOBase ------------------------------------------------------

    _MAX_READ = 1 << 20  # NFS servers cap READ well below 5 MiB

    @property
    def closed(self):
        return self._closed

    def readable(self):
        return self._readable

    def writable(self):
        return self._writable

    def seekable(self):
        return True

    def readinto(self, b):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._readable:
            raise io.UnsupportedOperation("not readable")
        if len(b) == 0:
            return 0
        length = min(len(b), self._MAX_READ)
        self.fs._check_read_allocation(length, self.path)
        data = self._pread(length)
        n = len(data)
        if n:
            b[:n] = data
            self._pos += n
        return n

    def read(self, size=-1):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._readable:
            raise io.UnsupportedOperation("not readable")
        if size is None or size < 0:
            if self._fd is None and self._pos == 0:
                # Whole-file read: one batched read, no size stat, no OPEN.
                data, errors = self.fs._client.read_all_many(
                    [self.fs._native_path(self.path)]
                )
                if errors:
                    raise _oserror(errors[0], self.path)
                buf = data[0] or b""
                self._pos = len(buf)
                self._cached_size = len(buf)
                return buf
            # One size fetch + one read loop instead of readall()'s
            # doubling-chunk preads.
            size = max(0, self._size() - self._pos)
            if size == 0:
                return b""
        self.fs._check_read_allocation(size, self.path)
        chunks = []
        remaining = size
        while remaining > 0:
            chunk = self._pread(min(remaining, self._MAX_READ))
            if not chunk:
                break
            chunks.append(chunk)
            self._pos += len(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)

    def write(self, data):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._writable:
            raise io.UnsupportedOperation("not writable")
        if isinstance(data, str):
            raise TypeError("a bytes-like object is required, not 'str'")
        self.fs._invalidate_persistent_caches([self.path])
        self.fs._invalidate_parent_listing(self.path)
        fd = self._ensure_open()
        try:
            if self._base_mode.startswith("a"):
                # O_APPEND: the backend appends regardless of the requested offset.
                n, new_pos = self.fs._client.write_positioned(fd, data)
            else:
                n = self.fs._client.pwrite(fd, data, self._pos)
                new_pos = self._pos + n
        except ConnectionError:
            # A mutation may already have reached the server. Reconnect for
            # future path operations, but never replay the write implicitly.
            self._broken = True
            try:
                if self.fs.auto_reconnect and self.fs._client.descriptor_valid(fd):
                    self.fs._client.reconnect_descriptor(fd)
            except BaseException:
                # Preserve the ambiguous mutation error; reconnect is cleanup.
                pass
            finally:
                self._fd = None
                self._fd_generation = None
            raise
        if self._cached_size is not None:
            self._cached_size = max(self._cached_size, new_pos)
        self._pos = new_pos
        return n

    def seek(self, offset, whence=0):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if whence == 0:
            new = offset
        elif whence == 1:
            new = self._pos + offset
        elif whence == 2:
            new = self._size() + offset
        else:
            raise ValueError(f"invalid whence: {whence}")
        if new < 0:
            raise OSError(errno.EINVAL, "Invalid argument")
        self._pos = new
        return new

    def tell(self):
        return self._pos

    def truncate(self, size=None):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._writable:
            raise io.UnsupportedOperation("not writable")
        if size is None:
            size = self._pos
        self.fs._invalidate_persistent_caches([self.path])
        self.fs._invalidate_parent_listing(self.path)
        self.fs._client.truncate(self.fs._native_path(self.path), size)
        self._cached_size = size
        return size

    def flush(self):
        # NFS writes are FILE_SYNC (stable); nothing to flush.
        return None

    def close(self):
        if self._closed:
            return
        fd = self._fd
        if (
            fd is not None
            and self.fs._client.descriptor_valid(fd)
            and not self.fs._client.closed
        ):
            # Retain ownership when CLOSE fails so callers (and __del__) can
            # retry cleanup instead of silently leaking server-side open state.
            self.fs._client.close(fd)
        self._fd = None
        self._fd_generation = None
        self._closed = True
        super().close()

    @property
    def size(self):
        return self._size()


class VfsiFile(AbstractBufferedFile):
    """fsspec-compatible buffered facade over a native VFSI descriptor.

    Standard read modes use fsspec's cache implementations. Writes remain
    write-through unless explicitly opted into buffering, while update modes
    containing ``+`` always retain the raw descriptor semantics.
    """

    DEFAULT_BLOCK_SIZE = 1 << 20

    def __init__(
        self,
        fs,
        path,
        mode="rb",
        fd=None,
        block_size=None,
        cache_type="readahead",
        cache_options=None,
        write_buffering=False,
        size=None,
        defer_cache=False,
    ):
        self._raw = _RawVfsiFile(fs, path, mode, fd=fd)
        self._base_mode = _normalize_mode(mode)
        self._buffered_read = self._base_mode == "r" and cache_type not in (
            None,
            "none",
        )
        self._buffered_write = self._base_mode in ("w", "a", "x") and bool(
            write_buffering
        )
        self._using_buffer = self._buffered_read or self._buffered_write
        self._buffer_group = None
        self._write_spool = None
        self._spool_read_offset = 0
        self._write_failed = False
        self._buffer_size = size
        self._requested_read_end = None
        self.cache_type = cache_type
        self._cache_options = dict(cache_options or {})
        self._deferred_cache_type = (
            cache_type if defer_cache and self._buffered_read else None
        )
        effective_block_size = block_size or self.DEFAULT_BLOCK_SIZE

        if self._using_buffer:
            super().__init__(
                fs,
                self._raw.path,
                mode=self._raw._native_mode(),
                block_size=effective_block_size,
                cache_type=(
                    "none" if self._deferred_cache_type is not None else cache_type
                ),
                cache_options=(
                    {} if self._deferred_cache_type is not None else self._cache_options
                ),
                size=size,
            )
            if self._buffered_write and self._base_mode == "a":
                append_size = self._raw.tell() if size is None else size
                self._raw._pos = append_size
                self._raw._cached_size = append_size
                self._buffer_size = append_size
                self.loc = append_size
        else:
            io.IOBase.__init__(self)
            self.fs = fs
            self.path = self._raw.path
            self.mode = mode
            self.blocksize = effective_block_size
            self.loc = self._raw.tell()
            self._closed = False

    @property
    def _fd(self):
        return self._raw._fd

    @_fd.setter
    def _fd(self, value):
        self._raw._fd = value

    @property
    def _fd_generation(self):
        return self._raw._fd_generation

    @_fd_generation.setter
    def _fd_generation(self, value):
        self._raw._fd_generation = value

    @property
    def size(self):
        if self._buffer_size is not None:
            return self._buffer_size
        return self._raw.size

    @size.setter
    def size(self, value):
        self._buffer_size = value

    def _attach_group(self, group):
        self._buffer_group = group
        if self._deferred_cache_type is not None:
            cache_type = self._deferred_cache_type
            self._deferred_cache_type = None
            self.cache = caches[cache_type](
                self.blocksize,
                self._fetch_range,
                self.size,
                **self._cache_options,
            )
        if self._buffered_write:
            threshold = max(
                1,
                min(
                    self.blocksize,
                    self.fs.max_batch_bytes // max(1, len(group.files)),
                ),
            )
            self._write_spool = tempfile.SpooledTemporaryFile(
                max_size=threshold, mode="w+b"
            )
            self._group_write_limit = threshold

    def _has_read_cache(self):
        return self._buffered_read or (
            self._base_mode == "r" and getattr(self, "cache", None) is not None
        )

    def _cache_is_pristine(self):
        if not self._buffered_read or self.cache_type in ("all", "parts"):
            return False
        cache = getattr(self, "cache", None)
        return cache is not None and not any(
            getattr(cache, field, 0)
            for field in ("hit_count", "miss_count", "total_requested_bytes")
        )

    def _fetch_range(self, start, end):
        self.fs._check_read_allocation(max(0, end - start), self.path)
        if self._buffer_group is not None and self.fs.vectorized_buffering:
            return self._buffer_group.fetch(self, start, end)
        return self._raw._pread_at(start, max(0, end - start))

    def _initiate_upload(self):
        # Creation/truncation and append positioning happen when the raw
        # descriptor is opened, preserving POSIX open-time errors.
        return None

    def _upload_chunk(self, final=False):
        data = self.buffer.getvalue()
        offset = 0
        try:
            while offset < len(data):
                written = self._raw.write(data[offset:])
                if written <= 0:
                    raise OSError(errno.EIO, "short buffered write", self.path)
                offset += written
        except ConnectionError:
            self._write_failed = True
            raise
        return True

    def _stage_buffer(self):
        if self.buffer.tell() == 0:
            return
        if self._write_spool is None:
            raise RuntimeError("grouped writer has no staging spool")
        self._write_spool.seek(0, io.SEEK_END)
        self._write_spool.write(self.buffer.getvalue())
        self.buffer = io.BytesIO()
        if self._buffer_group is not None:
            self._buffer_group.enforce_memory_limit()

    def read(self, size=-1):
        if not self._has_read_cache():
            return self._raw.read(size)
        if (
            self._buffered_read
            and self.cache_type != "blockcache"
            and (size is None or size < 0)
            and self.loc == 0
            and self._cache_is_pristine()
        ):
            if self.closed:
                raise ValueError("I/O operation on closed file")
            if self._buffer_group is not None and self.fs.vectorized_buffering:
                data = self._buffer_group.read_all(self)
            else:
                data, errors = self.fs._client.read_all_many(
                    [self.fs._native_path(self.path)]
                )
                if errors:
                    raise _oserror(errors[0], self.path)
                data = data[0] or b""
            self.loc = len(data)
            return data
        requested_end = (
            self.size if size is None or size < 0 else min(self.loc + size, self.size)
        )
        self.fs._check_read_allocation(max(0, requested_end - self.loc), self.path)
        self._requested_read_end = requested_end
        try:
            if size is not None and size >= 0:
                # fsspec before 2025 did not clamp oversized reads before
                # BlockCache calculated its inclusive end block. Clamp here
                # so it cannot request a block beyond the cache's nblocks.
                size = min(size, max(0, self.size - self.loc))
            return super().read(size)
        finally:
            self._requested_read_end = None

    def readinto(self, b):
        if not self._has_read_cache():
            return self._raw.readinto(b)
        return super().readinto(b)

    def write(self, data):
        if not self._using_buffer:
            return self._raw.write(data)
        if not self._buffered_write:
            return super().write(data)
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if isinstance(data, str):
            raise TypeError("a bytes-like object is required, not 'str'")
        if self._write_failed:
            raise ConnectionError("buffered writer is unusable after a failed flush")
        if self._buffer_group is None:
            written = self.buffer.write(data)
            self.loc += written
            complete = self.buffer.tell() // self.blocksize * self.blocksize
            if complete:
                payload = self.buffer.getvalue()
                self.buffer = io.BytesIO(payload[:complete])
                if self.offset is None:
                    self.offset = 0
                    self._initiate_upload()
                if self._upload_chunk(final=False) is not False:
                    self.offset += complete
                    self.buffer = io.BytesIO(payload[complete:])
                    self.buffer.seek(0, io.SEEK_END)
            return written
        view = memoryview(data).cast("B")
        written = 0
        while written < len(view):
            room = self._group_write_limit - self.buffer.tell()
            if room <= 0:
                self._stage_buffer()
                room = self._group_write_limit
            chunk = min(room, len(view) - written)
            self.buffer.write(view[written : written + chunk])
            written += chunk
            if self.buffer.tell() >= self._group_write_limit:
                self._stage_buffer()
        self.loc += written
        return written

    def seek(self, offset, whence=0):
        requested = int(offset)
        if whence == 0:
            position = requested
        elif whence == 1:
            position = self.tell() + requested
        elif whence == 2:
            position = self.size + requested
        else:
            position = 0
        if whence in (0, 1, 2) and position < 0:
            raise OSError(errno.EINVAL, "Invalid argument")
        if not self._has_read_cache() and not self._buffered_write:
            return self._raw.seek(requested, whence)
        return super().seek(requested, whence)

    def tell(self):
        if not self._has_read_cache() and not self._buffered_write:
            return self._raw.tell()
        return super().tell()

    def truncate(self, size=None):
        if not self._using_buffer:
            return self._raw.truncate(size)
        if self._buffered_write:
            self.flush()
            if size is None:
                size = self.tell()
            return self._raw.truncate(size)
        raise io.UnsupportedOperation("not writable")

    def flush(self, force=False):
        if not self._using_buffer:
            return self._raw.flush()
        if self._buffered_read:
            return None
        if self._write_failed:
            raise ConnectionError("buffered writer is unusable after a failed flush")
        if self._buffer_group is None:
            if force:
                return super().flush(force=True)
            if self.buffer.tell() == 0:
                return None
            if self.offset is None:
                self.offset = 0
                self._initiate_upload()
            buffered = self.buffer.seek(0, io.SEEK_END)
            if self._upload_chunk(final=False) is not False:
                self.offset += buffered
                self.buffer = io.BytesIO()
            return None
        if force:
            if self.forced:
                return None
            self.forced = True
        self._stage_buffer()
        self._buffer_group.flush_files([self])
        return None

    def readable(self):
        if not self._has_read_cache() and not self._buffered_write:
            return self._raw.readable()
        return super().readable()

    def writable(self):
        if not self._using_buffer:
            return self._raw.writable()
        return super().writable()

    def seekable(self):
        if not self._has_read_cache() and not self._buffered_write:
            return self._raw.seekable()
        return super().seekable()

    def close(self):
        if self.closed:
            group = self._buffer_group
            if group is not None and not group._group_closed:
                group.request_close(self)
            return
        if self._write_failed and self._raw._fd is None:
            # An ambiguous direct write already invalidated its descriptor, so
            # there is no cleanup ownership left to retain. Make close final
            # while still surfacing the original unusable-writer state.
            self._raw.close()
            self._closed = True
            raise ConnectionError("buffered writer is unusable after a failed flush")
        group = self._buffer_group
        if group is not None:
            if self._buffered_write:
                self.flush(force=True)
            elif self._has_read_cache():
                cache = getattr(self, "cache", None)
                close = getattr(cache, "close", None)
                if callable(close):
                    close()
                self.cache = None
            group.request_close(self)
            self._closed = True
            return
        if self._buffered_write:
            self.flush(force=True)
        elif self._has_read_cache():
            cache = getattr(self, "cache", None)
            close = getattr(cache, "close", None)
            if callable(close):
                close()
            self.cache = None
        self._raw.close()
        self._closed = True

    @property
    def closed(self):
        return getattr(self, "_closed", True)

    @closed.setter
    def closed(self, value):
        self._closed = value

    def _finish_group_close(self):
        self._raw._fd = None
        self._raw._fd_generation = None
        self._raw._closed = True
        self._closed = True
        if self._write_spool is not None:
            self._write_spool.close()
            self._write_spool = None


class _BufferGroup:
    """Coordinate cache misses and buffered flushes across OpenFiles."""

    def __init__(self, fs, files):
        self.fs = fs
        self.files = list(files)
        self._lock = threading.RLock()
        self._ranges = {}
        self._whole_files = {}
        self._close_requested = set()
        self._group_closed = False
        for file in self.files:
            file._attach_group(self)

    def _active_readers(self, current):
        current_cache = getattr(current, "cache", None)
        compatible = [
            file
            for file in self.files
            if not file.closed
            and file._has_read_cache()
            and type(getattr(file, "cache", None)) is type(current_cache)
            and file.blocksize == current.blocksize
        ]
        return [current] + [file for file in compatible if file is not current]

    def _has_speculative_data(self, file):
        identity = id(file)
        return identity in self._whole_files or any(
            key[0] == identity for key in self._ranges
        )

    def in_memory_bytes(self):
        with self._lock:
            total = 0
            for file in self.files:
                buffer = getattr(file, "buffer", None)
                if buffer is not None:
                    total += buffer.tell()
                spool = file._write_spool
                if spool is not None and not spool._rolled:
                    position = spool.tell()
                    total += spool.seek(0, io.SEEK_END)
                    spool.seek(position)
            return total

    def enforce_memory_limit(self):
        with self._lock:
            if self.in_memory_bytes() < self.fs.max_batch_bytes:
                return
            for file in self.files:
                spool = file._write_spool
                if spool is not None and not spool._rolled:
                    position = spool.tell()
                    size = spool.seek(0, io.SEEK_END)
                    spool.seek(position)
                    if size:
                        spool.rollover()

    def _reopen_readers(self):
        active = [file for file in self.files if not file.closed and file.readable()]
        if not active:
            return
        descriptor = next(
            (file._raw._fd for file in active if file._raw._fd is not None), None
        )
        if descriptor is not None and self.fs._client.descriptor_valid(descriptor):
            self.fs._client.reconnect_descriptor(descriptor)
        fds = self.fs._client.open_many(
            [self.fs._native_path(file.path) for file in active],
            [file._raw._native_mode() for file in active],
        )
        for file, fd in zip(active, fds):
            file._raw._fd = fd
            file._raw._fd_generation = self.fs._client.generation

    def _pread_many(self, files, offsets, lengths):
        fds = [file._raw._ensure_open() for file in files]
        try:
            return self.fs._client.pread_many(fds, offsets, lengths)
        except ConnectionError:
            if not self.fs.auto_reconnect:
                raise
            self._reopen_readers()
            fds = [file._raw._ensure_open() for file in files]
            return self.fs._client.pread_many(fds, offsets, lengths)

    def fetch(self, current, start, end):
        with self._lock:
            key = (id(current), start, end)
            prefetched = self._ranges.pop(key, None)
            if prefetched is not None:
                return prefetched
            # Persistent mmap fills must cover the fetcher's complete range
            # because they mark whole blocks present. Other fsspec caches may
            # safely consume a speculative prefix that covers the caller's
            # requested bytes and fetch more on a later miss.
            required_end = current._requested_read_end
            coverage_end = (
                end
                if getattr(getattr(current, "cache", None), "name", None) == "mmap"
                or required_end is None
                else required_end
            )
            for cached_key, value in list(self._ranges.items()):
                identity, cached_start, cached_end = cached_key
                if (
                    identity == id(current)
                    and cached_start <= start
                    and cached_end >= coverage_end
                ):
                    del self._ranges[cached_key]
                    offset = start - cached_start
                    if coverage_end == end:
                        return value[offset : offset + end - start]
                    return value[offset:]

            files = []
            offsets = []
            lengths = []
            total = 0
            for file in self._active_readers(current):
                if file is not current and end - start > 2 * current.blocksize:
                    continue
                if file is not current and self._has_speculative_data(file):
                    continue
                if file is not current and file.loc > start:
                    continue
                if len(files) >= self.fs.batch_size or start >= file.size:
                    continue
                request_end = min(end, file.size)
                if file is not current:
                    request_end = min(request_end, start + file.blocksize)
                length = request_end - start
                if length <= 0:
                    continue
                allocation_budget = min(
                    self.fs.max_batch_bytes, self.fs.read_all_max_total_bytes
                )
                if files and total + length > allocation_budget:
                    continue
                files.append(file)
                offsets.append(start)
                lengths.append(length)
                total += length

            data, errors = self._pread_many(files, offsets, lengths)
            current_data = None
            for index, (file, length, value) in enumerate(zip(files, lengths, data)):
                if index in errors:
                    if file is current:
                        raise _oserror(errors[index], current.path)
                    continue
                value = value or b""
                if file is current:
                    current_data = value
                else:
                    self._ranges[(id(file), start, start + length)] = value
            if current_data is None:
                raise OSError(errno.EIO, "buffered read returned no data", current.path)
            return current_data

    def read_all(self, current):
        with self._lock:
            self.fs._check_read_allocation(current.size, current.path)
            prefetched = self._whole_files.pop(id(current), None)
            if prefetched is not None:
                return prefetched
            # A whole-file read supersedes a speculative prefix left for this
            # handle. Drop it instead of retaining stale group memory.
            identity = id(current)
            for key in [key for key in self._ranges if key[0] == identity]:
                del self._ranges[key]
            files = []
            lengths = []
            total = 0
            for file in self._active_readers(current):
                if file is not current and self._has_speculative_data(file):
                    continue
                length = (
                    file.size if file is current else min(file.size, file.blocksize)
                )
                if len(files) >= self.fs.batch_size:
                    continue
                allocation_budget = min(
                    self.fs.max_batch_bytes, self.fs.read_all_max_total_bytes
                )
                if files and total + length > allocation_budget:
                    continue
                files.append(file)
                lengths.append(length)
                total += length
            data, errors = self._pread_many(files, [0] * len(files), lengths)
            current_data = None
            for index, (file, value) in enumerate(zip(files, data)):
                if index in errors:
                    if file is current:
                        raise _oserror(errors[index], current.path)
                    continue
                value = value or b""
                if file is current:
                    current_data = value
                elif len(value) == file.size:
                    self._whole_files[id(file)] = value
                else:
                    self._ranges[(id(file), 0, len(value))] = value
            if current_data is None:
                raise OSError(errno.EIO, "buffered read returned no data", current.path)
            return current_data

    def flush_files(self, files):
        pending = [
            file
            for file in files
            if file._write_spool is not None
            and file._spool_read_offset < file._write_spool.seek(0, io.SEEK_END)
        ]
        while pending:
            wave = []
            datas = []
            total = 0
            for file in pending:
                file._write_spool.seek(file._spool_read_offset)
                data = file._write_spool.read(
                    min(file.blocksize, self.fs.max_batch_bytes)
                )
                if not data:
                    continue
                if wave and (
                    len(wave) >= self.fs.batch_size
                    or total + len(data) > self.fs.max_batch_bytes
                ):
                    continue
                wave.append(file)
                datas.append(data)
                total += len(data)
            if not wave:
                break
            try:
                fds = [file._raw._ensure_open() for file in wave]
                if wave[0]._base_mode == "a":
                    results = self.fs._client.append_many(fds, datas)
                    for file, data, (written, position) in zip(wave, datas, results):
                        if written != len(data):
                            raise OSError(errno.EIO, "short buffered append", file.path)
                        file._raw._pos = position
                        file._raw._cached_size = position
                        file._spool_read_offset += written
                else:
                    offsets = [file._raw._pos for file in wave]
                    results = self.fs._client.pwrite_many(fds, offsets, datas)
                    for file, data, written in zip(wave, datas, results):
                        if written != len(data):
                            raise OSError(errno.EIO, "short buffered write", file.path)
                        file._raw._pos += written
                        if file._raw._cached_size is not None:
                            file._raw._cached_size = max(
                                file._raw._cached_size, file._raw._pos
                            )
                        file._spool_read_offset += written
                for file in wave:
                    self.fs._invalidate_parent_listing(file.path)
            except BaseException as error:
                index = getattr(error, "index", None)
                if isinstance(index, int) and 0 <= index < len(wave):
                    try:
                        error.filename = wave[index].path
                    except (AttributeError, TypeError):
                        pass
                for file in wave:
                    self.fs._invalidate_parent_listing(file.path)
                    file._write_failed = True
                    file._raw._broken = True
                raise
            pending = [
                file
                for file in pending
                if file._spool_read_offset < file._write_spool.seek(0, io.SEEK_END)
            ]

    def request_close(self, file):
        with self._lock:
            if self._group_closed:
                return
            self._close_requested.add(id(file))
            if len(self._close_requested) != len(self.files):
                return
            self.close_all()

    def close_all(self):
        with self._lock:
            if self._group_closed:
                return
            members = [
                member
                for member in self.files
                if member._raw._fd is not None
                and self.fs._client.descriptor_valid(member._raw._fd)
            ]
            fds = [member._raw._fd for member in members]
            if fds and not self.fs._client.closed:
                # Do not disarm members until the entire vector CLOSE has
                # succeeded. A failed close remains explicitly retryable.
                try:
                    self.fs._client.close_many(fds)
                except BaseException as error:
                    completed = getattr(error, "index", 0)
                    if isinstance(completed, int) and 0 <= completed <= len(members):
                        for member in members[:completed]:
                            member._finish_group_close()
                    raise
            self._group_closed = True
            for member in self.files:
                member._finish_group_close()
            self._ranges.clear()
            self._whole_files.clear()


class _DeferredWriteFile:
    """Disk-spooled transactional write staged beside its destination."""

    def __init__(self, fs, path, mode="wb"):
        self.fs = fs
        self.path = path  # internal path
        self.mode = mode
        self._spool = tempfile.SpooledTemporaryFile(
            max_size=fs.transaction_spool_threshold, mode="w+b"
        )
        self._closed = False
        self._committed = False
        self._prepared = False
        parent = posixpath.dirname(path) or "/"
        name = posixpath.basename(path) or "root"
        self.temp_path = posixpath.join(
            parent, f".nfs4fs-txn-{uuid.uuid4().hex}-{name}"
        )
        if "a" in mode and fs.exists(path):
            fs._copy_remote_to_fileobj(path, self._spool)
            self._spool.seek(0, io.SEEK_END)

    def writable(self):
        return True

    def readable(self):
        return False

    def seekable(self):
        return True

    def write(self, data):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        if isinstance(data, str):
            raise TypeError("a bytes-like object is required, not 'str'")
        return self._spool.write(data)

    def seek(self, offset, whence=0):
        return self._spool.seek(offset, whence)

    def tell(self):
        return self._spool.tell()

    def truncate(self, size=None):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        return self._spool.truncate(size)

    def flush(self):
        self._spool.flush()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def close(self):
        # The spool is retained until the transaction prepares or discards it.
        self._closed = True

    def prepare(self):
        """Upload to a same-directory temporary file without exposing target."""
        if self._prepared:
            return
        if "x" in self.mode and self.fs.exists(self.path):
            raise FileExistsError(errno.EEXIST, "File exists", self.path)
        try:
            self.fs._write_spooled(self.temp_path, self._spool)
        except Exception:
            try:
                self.fs.rm(self.temp_path)
            except OSError:
                pass
            raise
        self._prepared = True

    def finalize(self):
        if self._committed:
            return
        self.fs._client.rename_many(
            [(self.fs._native_path(self.temp_path), self.fs._native_path(self.path))]
        )
        self._prepared = False
        self._committed = True
        self._spool.close()

    def commit(self):
        if self._committed:
            return
        self.prepare()
        self.finalize()

    def discard(self):
        if self._prepared:
            try:
                self.fs.rm(self.temp_path)
            except FileNotFoundError:
                pass
            self._prepared = False
        self._spool.close()
        self._committed = True
        self._closed = True

    @property
    def closed(self):
        return self._closed


class _VfsiTransaction(Transaction):
    """Prepare all writes, then expose them with one batched rename."""

    def complete(self, commit=True):
        files = list(self.files)
        self.files.clear()
        fs = self.fs
        try:
            if not commit:
                for file in files:
                    file.discard()
                return
            prepared = []
            try:
                for file in files:
                    file.prepare()
                    prepared.append(file)
                if prepared:
                    fs._invalidate_namespace(file.path for file in prepared)
                    self.fs._client.rename_many(
                        [
                            (
                                self.fs._native_path(file.temp_path),
                                self.fs._native_path(file.path),
                            )
                            for file in prepared
                        ]
                    )
                for file in prepared:
                    file._prepared = False
                    file._committed = True
                    file._spool.close()
            except Exception:
                for file in prepared:
                    file.discard()
                for file in files[len(prepared) :]:
                    file.discard()
                raise
        finally:
            if fs is not None:
                fs._intrans = False
                fs._transaction = None
                # This custom transaction bypasses AbstractFileSystem's normal
                # end_transaction path, so flush its deferred invalidations.
                invalidated = list(fs._invalidated_caches_in_transaction)
                fs._invalidated_caches_in_transaction.clear()
                for path in invalidated:
                    fs.invalidate_cache(path)
            self.fs = None


class VfsiFileSystem(AbstractFileSystem):
    """An fsspec filesystem over a vectorized VFSI backend.

    Parameters
    ----------
    host: str
        Network filesystem server host (default 127.0.0.1).
    root: str
        Export-relative prefix ("chroot") all paths are resolved under, e.g.
        ``"git/vnfs_tests"``.
    backend: "nfs", "smb", or "dummy"
        ``dummy`` uses a local-directory implementation of the same vectorized
        API. ``smb`` connects to the SMB2/3 share named by ``share``.
    dummy_root: str or None
        Filesystem root for the dummy backend (a unique temp dir when None).
    block_size: int
        Default byte size for per-open read and opt-in write buffers.
    cache_type: str or None
        A cache registered by fsspec. ``"none"`` disables read buffering.
    cache_options: dict or None
        Default keyword arguments for the selected fsspec cache.
    write_buffering: bool
        Delay standard-mode writes until flush or close. Disabled by default.
    vectorized_buffering: bool
        Fan buffered OpenFiles reads and writes into bounded VFSI vectors.
    use_listings_cache: bool
        Cache directory listings when true. Disabled by default because NFS
        and SMB namespaces are commonly modified by other clients.
    listings_expiry_time: float or None
        Number of seconds a cached listing remains valid.
    max_paths: int or None
        Maximum number of directory paths retained by fsspec's ``DirCache``.
    read_all_max_total_bytes: int
        Maximum aggregate bytes returned by one whole-file vector read.
    directory_max_entries: int
        Maximum entries materialized by one directory listing or tree walk.
    directory_max_path_bytes: int
        Maximum aggregate path bytes materialized by a listing or tree walk.
    walk_max_depth: int
        Maximum recursive depth materialized by a native tree walk.
    connection_pool_size: int
        Independent native sessions used to overlap operations from threads.
    """

    protocol = "vfsi"
    root_marker = "/"
    transaction_type = _VfsiTransaction
    _native_module = None
    _supported_backends = frozenset({"nfs", "smb", "dummy"})

    def __init__(
        self,
        host="127.0.0.1",
        root="",
        backend="nfs",
        dummy_root=None,
        auto_mkdir=False,
        compound_size_limit=None,
        minor_version=None,
        share=None,
        username="",
        password="",
        domain="",
        batch_size=128,
        max_batch_bytes=64 * 1024 * 1024,
        transfer_chunk_size=8 * 1024 * 1024,
        transaction_spool_threshold=8 * 1024 * 1024,
        block_size=1 * 1024 * 1024,
        cache_type="readahead",
        cache_options=None,
        write_buffering=False,
        vectorized_buffering=True,
        connect_timeout=10.0,
        request_timeout=5.0,
        auto_reconnect=True,
        use_listings_cache=False,
        listings_expiry_time=None,
        max_paths=None,
        read_all_max_total_bytes=16 * 1024 * 1024,
        directory_max_entries=100_000,
        directory_max_path_bytes=16 * 1024 * 1024,
        walk_max_depth=128,
        authentication="auth_sys",
        service_principal=None,
        require_secure_authentication=False,
        connection_pool_size=1,
        **kwargs,
    ):
        if backend not in self._supported_backends:
            choices = ", ".join(repr(name) for name in sorted(self._supported_backends))
            raise ValueError(f"backend must be one of {choices}")
        native_module = type(self)._native_module
        if native_module is None:
            raise TypeError(
                "VfsiFileSystem is a backend base class; install and use "
                "a protocol adapter such as nfs4fs or vsmbfs"
            )
        if backend != "dummy" and not host:
            raise ValueError("host must not be empty for network backends")
        if compound_size_limit is not None and compound_size_limit <= 0:
            raise ValueError("compound_size_limit must be a positive integer")
        if minor_version not in (None, 1, 2):
            raise ValueError("minor_version must be 1, 2, or None")
        if authentication not in ("auth_sys", "krb5", "krb5i"):
            raise ValueError("authentication must be 'auth_sys', 'krb5', or 'krb5i'")
        if backend != "nfs" and (
            authentication != "auth_sys"
            or service_principal is not None
            or require_secure_authentication
        ):
            raise ValueError("secure authentication options are NFS-only")
        if service_principal is not None and not isinstance(service_principal, str):
            raise TypeError("service_principal must be a string or None")
        if service_principal is not None and authentication == "auth_sys":
            raise ValueError(
                "service_principal requires authentication='krb5' or 'krb5i'"
            )
        if require_secure_authentication and authentication == "auth_sys":
            raise ValueError(
                "require_secure_authentication requires authentication='krb5' or 'krb5i'"
            )
        for name, value in (
            ("batch_size", batch_size),
            ("max_batch_bytes", max_batch_bytes),
            ("transfer_chunk_size", transfer_chunk_size),
            ("transaction_spool_threshold", transaction_spool_threshold),
            ("block_size", block_size),
            ("connection_pool_size", connection_pool_size),
        ):
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise ValueError(f"{name} must be a positive integer")
        for name, value in (
            ("read_all_max_total_bytes", read_all_max_total_bytes),
            ("directory_max_entries", directory_max_entries),
            ("directory_max_path_bytes", directory_max_path_bytes),
            ("walk_max_depth", walk_max_depth),
        ):
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise ValueError(f"{name} must be a non-negative integer")
        if cache_type not in caches:
            choices = sorted(str(name) for name in caches if name is not None)
            raise ValueError(f"cache_type must be one of {choices}")
        if cache_options is not None and not isinstance(cache_options, dict):
            raise TypeError("cache_options must be a dict or None")
        for name, value in (
            ("connect_timeout", connect_timeout),
            ("request_timeout", request_timeout),
        ):
            if (
                not isinstance(value, (int, float))
                or isinstance(value, bool)
                or not math.isfinite(value)
                or value <= 0
            ):
                raise ValueError(f"{name} must be a positive number")
        root = "" if root is None else str(root)
        root_parts = [part for part in root.split("/") if part]
        if any(part in {".", ".."} for part in root_parts):
            raise ValueError("root must not contain '.' or '..' components")
        super().__init__(
            use_listings_cache=use_listings_cache,
            listings_expiry_time=listings_expiry_time,
            max_paths=max_paths,
            **kwargs,
        )
        self.host = host
        self.backend = backend
        # Like LocalFileSystem: write-mode operations create missing parent
        # directories only when auto_mkdir is set.
        self.auto_mkdir = auto_mkdir
        # Per-compound payload cap (bytes) for merged path I/O; None uses the
        # native default (1 MiB).
        self.compound_size_limit = compound_size_limit
        # None negotiates the highest supported version; 1 or 2 pins it.
        self.minor_version = minor_version
        self.share = share
        self.username = username
        self.domain = domain
        self.authentication = authentication
        self.service_principal = service_principal
        self.require_secure_authentication = bool(require_secure_authentication)
        self.connection_pool_size = connection_pool_size
        self.batch_size = batch_size
        self.max_batch_bytes = max_batch_bytes
        self.transfer_chunk_size = transfer_chunk_size
        self.transaction_spool_threshold = transaction_spool_threshold
        self.block_size = block_size
        self.cache_type = cache_type
        self.cache_options = dict(cache_options or {})
        self.write_buffering = bool(write_buffering)
        self.vectorized_buffering = bool(vectorized_buffering)
        self.connect_timeout = float(connect_timeout)
        self.request_timeout = float(request_timeout)
        self.auto_reconnect = bool(auto_reconnect)
        self.read_all_max_total_bytes = read_all_max_total_bytes
        self.directory_max_entries = directory_max_entries
        self.directory_max_path_bytes = directory_max_path_bytes
        self.walk_max_depth = walk_max_depth
        self._root = root.strip("/")
        self._client = _ClientPool(
            native_module,
            (
                host,
                backend,
                dummy_root,
                compound_size_limit,
                minor_version,
                share,
                username,
                password,
                domain,
                self.connect_timeout,
                self.request_timeout,
                self.read_all_max_total_bytes,
                self.directory_max_entries,
                self.directory_max_path_bytes,
                self.walk_max_depth,
                self.authentication,
                self.service_principal,
                self.require_secure_authentication,
            ),
            size=self.connection_pool_size,
            auto_reconnect=self.auto_reconnect,
        )
        self._dircache_lock = threading.RLock()
        self._dircache_generation = self._client.generation
        self._dircache_epoch = 0
        self._persistent_cache_lock = threading.RLock()
        self._persistent_cache_refs = []

    def _check_read_allocation(self, requested, path=None):
        """Bound every public operation that returns newly allocated bytes."""
        requested = max(0, int(requested))
        if requested > self.read_all_max_total_bytes:
            raise _allocation_error(path, requested, self.read_all_max_total_bytes)

    def _stream_read_size(self):
        """Bound one streaming buffer without imposing an aggregate file cap."""
        return max(1, min(self.transfer_chunk_size, self.read_all_max_total_bytes))

    @property
    def closed(self):
        """Whether this filesystem's native session has been released."""
        return self._client.closed

    def close(self):
        """Release the native connection and evict this cached instance."""
        with self._dircache_lock:
            self._dircache_epoch += 1
            self.dircache.clear()
        if not self._client.closed:
            self._client.shutdown()
        token = getattr(self, "_fs_token_", None)
        if token is not None:
            type(self)._cache.pop(token, None)

    def __enter__(self):
        if self.closed:
            raise ValueError("filesystem is closed")
        return self

    def __exit__(self, exc_type, exc_value, traceback):
        self.close()

    def smb_dialect(self):
        """Return the negotiated SMB dialect revision, or ``None``."""
        return self._client.smb_dialect()

    # -- path handling -----------------------------------------------------

    @classmethod
    def _get_kwargs_from_urls(cls, urlpath):
        """Extract a server from ``nfs4://server/path`` style URLs.

        Credentials remain explicit constructor options so they do not leak
        through logs, tracebacks, or copied URLs. A port is retained for SMB,
        whose native backend accepts ``host:port``.
        """
        if not isinstance(urlpath, str):
            return {}
        parsed = urlsplit(urlpath)
        protos = (cls.protocol,) if isinstance(cls.protocol, str) else cls.protocol
        if parsed.scheme not in protos or not parsed.netloc:
            return {}
        if parsed.username is not None or parsed.password is not None:
            raise ValueError("credentials in VFSI URLs are not supported")
        if parsed.query or parsed.fragment:
            raise ValueError("query strings and fragments are not supported")
        host = parsed.hostname
        if host is None:
            return {}
        if ":" in host:
            host = f"[{host}]"
        if parsed.port is not None:
            host = f"{host}:{parsed.port}"
        return {"host": host}

    @classmethod
    def _strip_protocol(cls, path):
        if isinstance(path, list):
            return [cls._strip_protocol(p) for p in path]
        if not isinstance(path, str):
            path = str(path)
        protos = (cls.protocol,) if isinstance(cls.protocol, str) else cls.protocol
        parsed = urlsplit(path)
        if parsed.scheme in protos:
            if parsed.query or parsed.fragment:
                raise ValueError("query strings and fragments are not supported")
            path = unquote(parsed.path)
        for protocol in protos:
            if path.startswith(protocol + "://"):
                path = path[len(protocol) + 3 :]
            elif path.startswith(protocol + "::"):
                path = path[len(protocol) + 2 :]
        if not path.startswith("/"):
            path = "/" + path
        path = posixpath.normpath(path).rstrip("/")
        return path or cls.root_marker

    def _native_path(self, internal):
        """Internal ('/a/b') -> root-prefixed path for the native client."""
        if self._root:
            if internal == "/":
                return "/" + self._root
            return "/" + self._root + internal
        return internal

    def _internalize(self, native):
        """Root-prefixed native path -> internal ('/a/b')."""
        if self._root:
            prefix = "/" + self._root
            if native == prefix:
                return "/"
            if native.startswith(prefix + "/"):
                return native[len(prefix) :]
        return native

    def _fullpath(self, internal):
        """Internal ('/a/b') -> a full path for this registered protocol."""
        protocol = self.protocol if isinstance(self.protocol, str) else self.protocol[0]
        return protocol + "://" + internal

    # -- directory listing cache -----------------------------------------

    def _sync_dircache_generation(self):
        """Reject closed clients and discard cache state after reconnects."""
        self._client.ensure_ready()
        generation = self._client.generation
        with self._dircache_lock:
            if generation != self._dircache_generation:
                self._dircache_epoch += 1
                self.dircache.clear()
                self._dircache_generation = generation

    def _dircache_epoch_snapshot(self):
        """Return an epoch that a native listing must match before caching."""
        self._sync_dircache_generation()
        with self._dircache_lock:
            return self._dircache_epoch

    @staticmethod
    def _copy_listing(infos):
        """Keep callers from mutating dictionaries retained in ``DirCache``."""
        return [dict(info) for info in infos]

    def _cached_listing(self, internal):
        self._sync_dircache_generation()
        with self._dircache_lock:
            try:
                infos = self.dircache[internal]
            except KeyError:
                return None
            return self._copy_listing(infos)

    def _store_listing(self, internal, infos, fill_epoch):
        # A native operation may have reconnected transparently. Clear cache
        # entries from the old session before retaining its fresh result.
        self._sync_dircache_generation()
        stored = self._copy_listing(infos)
        with self._dircache_lock:
            if fill_epoch != self._dircache_epoch:
                return False
            self.dircache[internal] = stored
            return True

    def _evict_dircache(self, subtrees=(), exact=()):
        """Evict many cache keys atomically with one generation bump/scan."""
        subtrees = set(subtrees)
        exact = set(exact)
        with self._dircache_lock:
            self._dircache_epoch += 1
            if "/" in subtrees:
                self.dircache.clear()
                return
            if not subtrees:
                for key in exact:
                    self.dircache.pop(key, None)
                return
            for key in list(self.dircache):
                candidate = key
                in_subtree = False
                while candidate != "/":
                    if candidate in subtrees:
                        in_subtree = True
                        break
                    candidate = posixpath.dirname(candidate.rstrip("/")) or "/"
                if key in exact or in_subtree:
                    self.dircache.pop(key, None)

    def _register_persistent_cache(self, cache_fs, invalidator):
        """Register a cache wrapper for same-client mutation notifications."""
        with self._persistent_cache_lock:
            retained = []
            found = False
            for cache_ref, callback in self._persistent_cache_refs:
                cache = cache_ref()
                if cache is None:
                    continue
                retained.append((cache_ref, callback))
                if cache is cache_fs:
                    found = True
            if not found:
                retained.append((weakref.ref(cache_fs), invalidator))
            self._persistent_cache_refs = retained

    def _invalidate_persistent_caches(self, paths, subtrees=False):
        """Make registered persistent data generations stale before mutation."""
        internals = {self._strip_protocol(path) for path in paths}
        if not internals:
            return
        with self._persistent_cache_lock:
            retained = []
            caches = []
            for cache_ref, invalidator in self._persistent_cache_refs:
                cache = cache_ref()
                if cache is None:
                    continue
                retained.append((cache_ref, invalidator))
                caches.append((cache, invalidator))
            self._persistent_cache_refs = retained
        for cache, invalidator in caches:
            invalidator(cache, internals, subtrees=subtrees)

    def invalidate_cache(self, path=None):
        """Discard a cached listing and all cached descendants.

        This follows fsspec's public contract for ``path`` while still calling
        the base method so invalidations are replayed after transactions.
        """
        internal = None if path is None else self._strip_protocol(path)
        self._evict_dircache(subtrees={"/" if internal is None else internal})
        super().invalidate_cache(internal)

    def _invalidate_parent_listing(self, path):
        """Discard only the listing containing ``path``."""
        internal = self._strip_protocol(path)
        parent = posixpath.dirname(internal.rstrip("/")) or "/"
        self._evict_dircache(exact={parent})
        # Preserve fsspec's deferred transaction invalidation behavior. Its
        # public invalidation is broader when replayed, which is conservative.
        super().invalidate_cache(parent)

    def _invalidate_namespace(self, paths):
        """Invalidate object subtrees, parents, and cached parent metadata."""
        internals = {self._strip_protocol(path) for path in paths}
        if not internals:
            return
        self._invalidate_persistent_caches(internals, subtrees=True)
        parents = {
            posixpath.dirname(internal.rstrip("/")) or "/" for internal in internals
        }
        # A namespace mutation changes the containing directory's own mtime,
        # change attribute, and sometimes nlink. Those attributes are cached
        # in the listing one level above the containing directory.
        parent_containers = {
            posixpath.dirname(parent.rstrip("/")) or "/" for parent in parents
        }
        self._evict_dircache(subtrees=internals, exact=parents | parent_containers)
        for internal in internals | parents | parent_containers:
            super().invalidate_cache(internal)

    def _infos_from_native_entries(self, entries):
        infos = [
            _info_dict(self._fullpath(self._internalize(entry["name"])), entry)
            for entry in entries
        ]
        infos.sort(key=lambda info: info["name"])
        return infos

    def _cached_walk_tree(self, root, maxdepth):
        """Return a complete cached subtree, or ``None`` on any cache miss."""
        limit = None if maxdepth is None else _depth(root) + maxdepth
        tree = []
        pending = [root]
        while pending:
            directory = pending.pop()
            infos = self._cached_listing(directory)
            if infos is None:
                return None
            tree.append((directory, infos))
            children = []
            for info in infos:
                if info["type"] != "directory":
                    continue
                child = self._strip_protocol(info["name"])
                if limit is None or _depth(child) <= limit:
                    children.append(child)
            pending.extend(reversed(children))
        return tree

    def _native_walk_tree(self, internal, fill_epoch, maxdepth=None):
        if maxdepth is None:
            native_tree = self._client.walk(self._native_path(internal), sort=True)
        else:
            native_tree = self._client.walk(
                self._native_path(internal),
                sort=True,
                max_depth=min(self.walk_max_depth, maxdepth),
            )
        self._sync_dircache_generation()
        tree = []
        for native_dir, entries in native_tree:
            directory = self._internalize(native_dir)
            infos = self._infos_from_native_entries(entries)
            self._store_listing(directory, infos, fill_epoch)
            tree.append((directory, infos))
        return tree

    # -- batched directory creation ---------------------------------------

    def _ensure_dirs(self, paths, mode=0o755):
        """``mkdir -p`` a set of internal paths with as few compounds as
        possible: one batched existence probe, then one mkdirv batch per
        missing depth level."""
        paths = list(dict.fromkeys(p for p in paths if p not in ("",)))
        if not paths:
            return
        native_paths = [self._native_path(p) for p in paths]
        prefixes = set()
        for native in native_paths:
            comps = [c for c in native.split("/") if c]
            prefixes.update("/" + "/".join(comps[:i]) for i in range(1, len(comps) + 1))
        ordered = sorted(prefixes, key=lambda p: (p.count("/"), p))
        exists = self._client.exists_many(list(ordered))
        missing = [p for p, ok in zip(ordered, exists) if not ok]
        by_depth = {}
        for p in missing:
            by_depth.setdefault(p.count("/"), []).append(p)
        internal_missing = [
            internal
            for native in missing
            if self._native_path(internal := self._internalize(native)) == native
        ]
        self._invalidate_namespace(internal_missing)
        for depth in sorted(by_depth):
            self._client.mkdir_many(by_depth[depth], mode)

    def _makedirs_batched(self, paths, exist_ok=False):
        """fsspec ``makedirs`` semantics for many paths, batched."""
        paths = list(dict.fromkeys(self._strip_protocol(p) for p in paths))
        if not paths:
            return
        native = [self._native_path(p) for p in paths]
        lstats, lerrs = self._client.lstat_many(native)
        missing = []
        existing = []
        for i, p in enumerate(paths):
            if lstats[i] is not None:
                existing.append((i, p))
            else:
                code = lerrs.get(i)
                if code is not None and code != errno.ENOENT:
                    raise _oserror(code, p)
                missing.append(p)
        if existing and not exist_ok:
            raise FileExistsError(errno.EEXIST, f"File exists: {existing[0][1]!r}")
        if existing:
            estats, eerrs = self._client.stat_many(
                [self._native_path(p) for _, p in existing]
            )
            for j, (_, p) in enumerate(existing):
                if estats[j] is not None and estats[j]["type"] == "directory":
                    continue
                if j in eerrs and eerrs[j] != errno.ENOENT:
                    raise _oserror(eerrs[j], p)
                raise FileExistsError(errno.EEXIST, f"File exists: {p!r}")
        if missing:
            self._ensure_dirs(missing, 0o755)

    # -- metadata ----------------------------------------------------------

    def info(self, path, **kwargs):
        internal = self._strip_protocol(path)
        stats, errors = self._client.stat_many([self._native_path(internal)])
        if errors:
            raise _oserror(errors[0], internal)
        return _info_dict(self._fullpath(internal), stats[0])

    def _info_many(self, paths):
        """Return metadata for several paths in one vector operation."""
        internals = [self._strip_protocol(path) for path in paths]
        stats, errors = self._client.stat_many(
            [self._native_path(path) for path in internals]
        )
        if errors:
            index = min(errors)
            raise _oserror(errors[index], internals[index])
        return [
            _info_dict(self._fullpath(path), stat)
            for path, stat in zip(internals, stats)
        ]

    def ls(self, path, detail=True, refresh=False, **kwargs):
        internal = self._strip_protocol(path)
        if not refresh:
            cached = self._cached_listing(internal)
            if cached is not None:
                if detail:
                    return cached
                return [info["name"] for info in cached]
        else:
            self._evict_dircache(exact={internal})
        fill_epoch = self._dircache_epoch_snapshot()
        try:
            entries = self._client.listdir(self._native_path(internal))
        except NotADirectoryError:
            # The path is a file (or symlink to one): report it directly.
            info = self.info(internal, **kwargs)
            return [info] if detail else [info["name"]]
        infos = self._infos_from_native_entries(entries)
        self._store_listing(internal, infos, fill_epoch)
        if detail:
            return self._copy_listing(infos)
        return [d["name"] for d in infos]

    def exists(self, path, **kwargs):
        try:
            internal = self._strip_protocol(path)
            return self._client.exists_many([self._native_path(internal)])[0]
        except (FileNotFoundError, NotADirectoryError):
            return False

    def isfile(self, path):
        try:
            return self.info(path)["type"] == "file"
        except (FileNotFoundError, NotADirectoryError):
            return False

    def isdir(self, path):
        try:
            return self.info(path)["type"] == "directory"
        except (FileNotFoundError, NotADirectoryError):
            return False

    def size(self, path):
        return self.info(path).get("size")

    def sizes(self, paths):
        """Size of each path in one stat_many batch."""
        internals = [self._strip_protocol(p) for p in paths]
        stats, errors = self._client.stat_many(
            [self._native_path(p) for p in internals]
        )
        out = []
        for i, p in enumerate(internals):
            if stats[i] is None:
                raise _oserror(errors[i], p)
            out.append(stats[i]["size"])
        return out

    def created(self, path):
        ts = self.info(path).get("created")
        return (
            datetime.datetime.fromtimestamp(ts, tz=datetime.timezone.utc)
            if ts is not None
            else None
        )

    def modified(self, path):
        ts = self.info(path).get("modified")
        return (
            datetime.datetime.fromtimestamp(ts, tz=datetime.timezone.utc)
            if ts is not None
            else None
        )

    def ukey(self, path):
        return self._ukey_from_info(self.info(path))

    @classmethod
    def _ukey_from_info(cls, info):
        # Like LocalFileSystem (a hash of `info`, which includes the name),
        # the key changes when the file is renamed as well as when its
        # contents change.
        return hashlib.sha256(
            f"{info.get('name')}:{cls._version_token(info)}".encode()
        ).hexdigest()

    @staticmethod
    def _version_token(info):
        """Best available identity for one version of a filesystem object."""
        change = info.get("change")
        if change is not None:
            return f"change:{change}:fileid:{info.get('fileid')}"
        return (
            f"fileid:{info.get('fileid')}:mtime_ns:{info.get('modified_ns')}:"
            f"ctime_ns:{info.get('created_ns')}:size:{info.get('size')}"
        )

    def checksum(self, path):
        # A stable file-version value. NFS FATTR4_CHANGE is preferred; the
        # nanosecond metadata fallback covers local/SMB backends.
        info = self.info(path)
        digest = hashlib.sha256(self._version_token(info).encode()).hexdigest()
        return int(digest, 16)

    # -- bulk read / write -------------------------------------------------

    def _cat_batch(self, paths, on_error, callback=DEFAULT_CALLBACK):
        """Read path groups bounded by both item count and aggregate bytes."""
        callback.set_size(len(paths))
        native = [self._native_path(path) for path in paths]
        stats, stat_errors = self._client.stat_many(native)
        data = [b""] * len(paths)
        failures = {
            index: _oserror(code, paths[index]) for index, code in stat_errors.items()
        }
        reported = set()
        valid = [index for index in range(len(paths)) if index not in failures]
        valid_sizes = [stats[index].get("size", 0) for index in valid]
        self._check_read_allocation(sum(valid_sizes), paths[0] if paths else None)
        for positions in _bounded_batches(
            valid_sizes, self.batch_size, self.max_batch_bytes
        ):
            batch = [valid[position] for position in positions]
            if len(batch) == 1 and valid_sizes[positions[0]] > self.max_batch_bytes:
                index = batch[0]
                try:
                    with callback.branched(paths[index], _MEMORY_PATH) as child:
                        child.set_size(valid_sizes[positions[0]])
                        data[index] = self._read_one_streamed(
                            paths[index], callback=child
                        )
                except OSError as exc:
                    failures[index] = exc
                callback.relative_update()
                reported.add(index)
                continue
            dat, batch_errors = self._client.read_all_many(
                [native[index] for index in batch]
            )
            for position, value in enumerate(dat):
                index = batch[position]
                if value is not None:
                    data[index] = value
            for position, code in batch_errors.items():
                index = batch[position]
                failures[index] = _oserror(code, paths[index])
        out = {}
        for i, p in enumerate(paths):
            if i not in reported:
                if i in failures:
                    callback.relative_update()
                else:
                    _complete_child(callback, p, _MEMORY_PATH, len(data[i]))
            if i in failures:
                exc = failures[i]
                if on_error == "raise":
                    raise exc
                if on_error == "return":
                    out[p] = exc
                # "omit": skip the failed key
            else:
                out[p] = data[i]
        return out

    def _read_one_streamed(self, path, callback=DEFAULT_CALLBACK):
        """Read one result incrementally within the public allocation cap."""
        output = io.BytesIO()
        self._copy_remote_to_fileobj(path, output, callback=callback)
        return output.getvalue()

    def cat(
        self,
        path,
        recursive=False,
        on_error="raise",
        callback=DEFAULT_CALLBACK,
        **kwargs,
    ):
        callback = Callback.as_callback(callback)
        if isinstance(path, str):
            paths = self.expand_path(path, recursive=recursive, **kwargs)
            if len(paths) == 1 and paths[0] == self._strip_protocol(path):
                # Single literal path: cat_file semantics (raise on error).
                return self.cat_file(paths[0], callback=callback, **kwargs)
            return self._cat_batch(paths, on_error, callback=callback)
        paths = [self._strip_protocol(p) for p in path]
        if recursive:
            expanded = []
            for p in paths:
                expanded.extend(self.find(p, withdirs=False))
            paths = expanded
        return self._cat_batch(paths, on_error, callback=callback)

    def cat_file(self, path, start=None, end=None, callback=DEFAULT_CALLBACK, **kwargs):
        callback = Callback.as_callback(callback)
        internal = self._strip_protocol(path)
        size = None
        if (start is not None and start < 0) or (end is not None and end < 0):
            size = self.size(internal)
        try:
            start, end = _normalize_range(start, end, size)
        except ValueError:
            # LocalFileSystem opens before validating the read length, so a
            # missing path or directory takes precedence over a bad range.
            info = self.info(internal)
            if info["type"] == "directory":
                raise IsADirectoryError(errno.EISDIR, "Is a directory", internal)
            raise
        if start == 0 and end is None:
            # Whole file: read_allv skips the size stat entirely.
            data, errors = self._client.read_all_many([self._native_path(internal)])
            if errors:
                raise _oserror(errors[0], internal)
            result = data[0] or b""
            _complete_callback(callback, len(result))
            return result
        if end is None:
            size = self.size(internal) if size is None else size
            requested = max(0, size - start)
        else:
            requested = max(0, end - start)
        self._check_read_allocation(requested, internal)
        data, errors = self._client.read_many(
            [self._native_path(internal)], [start], [end]
        )
        if errors:
            raise _oserror(errors[0], internal)
        result = data[0] or b""
        _complete_callback(callback, len(result))
        return result

    def cat_ranges(
        self,
        paths,
        starts,
        ends,
        max_gap=None,
        on_error="return",
        callback=DEFAULT_CALLBACK,
        **kwargs,
    ):
        callback = Callback.as_callback(callback)
        if max_gap is not None:
            raise NotImplementedError("max_gap is not supported")
        if not isinstance(paths, list):
            raise TypeError("paths must be a list")
        if not isinstance(starts, list):
            starts = [starts] * len(paths)
        if not isinstance(ends, list):
            ends = [ends] * len(paths)
        if len(starts) != len(paths) or len(ends) != len(paths):
            raise ValueError("starts/ends must match paths")
        callback.set_size(len(paths))
        internals = [self._strip_protocol(p) for p in paths]
        native = [self._native_path(p) for p in internals]
        errors = {}
        sizes = [None] * len(paths)
        if any(
            (s is not None and s < 0)
            or e is None
            or e < 0
            or e == (0 if s is None else s) - 1
            for s, e in zip(starts, ends)
        ):
            # Slice semantics and byte-bounded batching both need file sizes.
            stats, stat_errors = self._client.stat_many(native)
            errors.update(stat_errors)
            sizes = [s["size"] if s is not None else None for s in stats]
        normalized = []
        validation_errors = {}
        for i, (start, end) in enumerate(zip(starts, ends)):
            if i in errors:
                normalized.append((0, 0))
                continue
            try:
                normalized.append(_normalize_range(start, end, sizes[i]))
            except ValueError as exc:
                try:
                    info = self.info(internals[i])
                    if info["type"] == "directory":
                        raise IsADirectoryError(
                            errno.EISDIR, "Is a directory", internals[i]
                        )
                except (OSError, ValueError) as path_error:
                    validation_errors[i] = path_error
                else:
                    validation_errors[i] = exc
                normalized.append((0, 0))
        if validation_errors and on_error != "return":
            raise validation_errors[min(validation_errors)]
        valid = [
            i
            for i in range(len(paths))
            if i not in errors and i not in validation_errors
        ]
        data = [None] * len(paths)
        lengths = [
            max(
                0,
                (sizes[i] if normalized[i][1] is None else normalized[i][1])
                - normalized[i][0],
            )
            for i in valid
        ]
        self._check_read_allocation(
            sum(lengths), internals[valid[0]] if valid else None
        )
        for positions in _bounded_batches(
            lengths, self.batch_size, self.max_batch_bytes
        ):
            batch = [valid[position] for position in positions]
            valid_data, read_errors = self._client.read_many(
                [native[i] for i in batch],
                [normalized[i][0] for i in batch],
                [normalized[i][1] for i in batch],
            )
            for j, i in enumerate(batch):
                data[i] = valid_data[j]
                if j in read_errors:
                    errors[i] = read_errors[j]
        out = []
        for i, p in enumerate(internals):
            if i in validation_errors:
                out.append(validation_errors[i])
                callback.relative_update()
            elif i in errors:
                exc = _oserror(errors[i], p)
                if on_error == "return":
                    out.append(exc)
                    callback.relative_update()
                else:
                    callback.relative_update()
                    raise exc
            else:
                result = data[i] or b""
                out.append(result)
                _complete_child(callback, p, _MEMORY_PATH, len(result))
        return out

    def _write_batch(
        self,
        paths,
        values,
        mode="overwrite",
        callback=None,
        callback_pairs=None,
    ):
        self._invalidate_namespace(paths)
        native = [self._native_path(p) for p in paths]
        datas = [v if isinstance(v, bytes) else bytes(v) for v in values]
        is_bulk = callback_pairs is not None
        if callback is not None:
            callback.set_size(len(paths) if is_bulk else len(datas[0]))

        def report(index, action=None):
            size = len(datas[index])
            if callback is None:
                if action is not None:
                    action(None)
                return
            if is_bulk:
                with callback.branched(*callback_pairs[index]) as child:
                    child.set_size(size)
                    if action is None:
                        child.relative_update(size)
                    else:
                        action(child)
                callback.relative_update()
            elif action is None:
                callback.relative_update(size)
            else:
                action(callback)

        if mode == "create":
            for index, path in enumerate(paths):
                report(
                    index,
                    lambda child, i=index, p=path: self._write_one_exclusive(
                        p, datas[i], callback=child
                    ),
                )
            return
        if mode != "overwrite":
            raise ValueError("mode must be 'overwrite' or 'create'")
        for batch in _bounded_batches(
            [len(data) for data in datas], self.batch_size, self.max_batch_bytes
        ):
            if len(batch) == 1 and len(datas[batch[0]]) > self.max_batch_bytes:
                index = batch[0]
                report(
                    index,
                    lambda child, i=index: self._write_one_streamed(
                        paths[i], datas[i], callback=child
                    ),
                )
                continue
            batch_native = [native[i] for i in batch]
            batch_datas = [datas[i] for i in batch]
            try:
                self._client.write_many(batch_native, batch_datas, truncate=True)
            except FileNotFoundError:
                if not self.auto_mkdir:
                    raise
                parents = {
                    posixpath.dirname(paths[i])
                    for i in batch
                    if posixpath.dirname(paths[i]) != "/"
                }
                self._ensure_dirs(list(parents), 0o755)
                self._client.write_many(batch_native, batch_datas, truncate=True)
            for index in batch:
                report(index)

    def _write_one_streamed(self, path, value, callback=None):
        """Overwrite one in-memory value with bounded native write calls."""
        internal = self._strip_protocol(path)
        parent = posixpath.dirname(internal)
        if self.auto_mkdir and parent not in ("", "/"):
            self._ensure_dirs([parent], 0o755)
        view = memoryview(value)
        with VfsiFile(self, internal, "wb") as remote:
            for offset in range(0, len(view), self.transfer_chunk_size):
                written = remote.write(
                    bytes(view[offset : offset + self.transfer_chunk_size])
                )
                if callback is not None:
                    callback.relative_update(written)

    def _write_one_exclusive(self, path, value, callback=None):
        """Atomically create one path with O_EXCL and bounded writes."""
        internal = self._strip_protocol(path)
        parent = posixpath.dirname(internal)
        if self.auto_mkdir and parent not in ("", "/"):
            self._ensure_dirs([parent], 0o755)
        fd = self._client.open(self._native_path(internal), "xb")
        completed = False
        write_error = None
        try:
            view = memoryview(value)
            offset = 0
            while offset < len(view):
                chunk = view[offset : offset + self.transfer_chunk_size]
                written = self._client.pwrite(fd, bytes(chunk), offset)
                if written <= 0:
                    raise OSError(errno.EIO, "short exclusive write", internal)
                offset += written
                if callback is not None:
                    callback.relative_update(written)
            completed = True
        except ConnectionError as exc:
            # The server may have completed the create or the last write.
            # A compensating remove could delete a later creator's file, so
            # leave resolution of this ambiguous result to the caller.
            completed = True
            write_error = exc
            raise
        except BaseException as exc:
            write_error = exc
            raise
        finally:
            try:
                self._client.close(fd)
            except BaseException:
                # Preserve the mutation error if both the write and CLOSE fail.
                if write_error is None:
                    raise
            finally:
                if not completed:
                    try:
                        self.rm(internal)
                    except OSError:
                        pass

    def _write_spooled(self, path, spool):
        """Stream a seekable local spool to one remote path."""
        spool.seek(0)
        with VfsiFile(self, path, "xb") as remote:
            while True:
                chunk = spool.read(self.transfer_chunk_size)
                if not chunk:
                    break
                remote.write(chunk)

    def _copy_remote_to_fileobj(self, path, output, callback=None):
        """Stream one remote file into a writable local file object."""
        with self.open(path, "rb", cache_type="none") as remote:
            while True:
                chunk = remote.read(self._stream_read_size())
                if not chunk:
                    break
                output.write(chunk)
                if callback is not None:
                    callback.relative_update(len(chunk))

    def pipe(self, path, value=None, callback=DEFAULT_CALLBACK, **kwargs):
        callback = Callback.as_callback(callback)
        if isinstance(path, str):
            mode = kwargs.pop("mode", "overwrite")
            return self.pipe_file(
                path,
                value if value is not None else b"",
                mode=mode,
                callback=callback,
                **kwargs,
            )
        elif isinstance(path, dict):
            paths = [self._strip_protocol(k) for k in path]
            values = list(path.values())
        else:
            raise ValueError("path must be str or dict")
        self._write_batch(
            paths,
            values,
            kwargs.get("mode", "overwrite"),
            callback=callback,
            callback_pairs=[(_MEMORY_PATH, path) for path in paths],
        )

    def pipe_file(
        self, path, value, mode="overwrite", callback=DEFAULT_CALLBACK, **kwargs
    ):
        callback = Callback.as_callback(callback)
        self._write_batch(
            [self._strip_protocol(path)], [value], mode, callback=callback
        )

    # -- batched local <-> remote transfer --------------------------------

    def get(
        self,
        rpath,
        lpath,
        recursive=False,
        callback=DEFAULT_CALLBACK,
        maxdepth=None,
        **kwargs,
    ):
        """Copy remote files to local, fetching every file in one read_allv
        batch instead of one round trip per file."""
        callback = Callback.as_callback(callback)
        from fsspec.implementations.local import (
            LocalFileSystem,
            make_path_posix,
            trailing_sep,
        )
        from fsspec.utils import other_paths

        pre_stats = None
        if isinstance(lpath, list) and isinstance(rpath, list):
            rpaths = rpath
            lpaths = lpath
        else:
            source_is_str = isinstance(rpath, str)
            rpaths = self.expand_path(
                rpath, recursive=recursive, maxdepth=maxdepth, **kwargs
            )
            if source_is_str and (not recursive or maxdepth is not None):
                candidates = [p for p in rpaths if not trailing_sep(p)]
                if not candidates:
                    return
                cstats, cerrs = self._client.stat_many(
                    [self._native_path(self._strip_protocol(p)) for p in candidates]
                )
                if cerrs:
                    i = min(cerrs)
                    raise _oserror(cerrs[i], candidates[i])
                pre_stats = {p: s for p, s in zip(candidates, cstats) if s is not None}
                rpaths = [
                    p
                    for p, s in zip(candidates, cstats)
                    if s is not None and s["type"] != "directory"
                ]
                if not rpaths:
                    return
            if isinstance(lpath, str):
                lpath = make_path_posix(lpath)
            source_is_file = len(rpaths) == 1
            dest_is_dir = isinstance(lpath, str) and (
                trailing_sep(lpath) or LocalFileSystem().isdir(lpath)
            )
            exists = source_is_str and (
                (has_magic(rpath) and source_is_file)
                or (not has_magic(rpath) and dest_is_dir and not trailing_sep(rpath))
            )
            lpaths = other_paths(
                rpaths,
                lpath,
                exists=exists,
                flatten=not source_is_str,
            )

        callback.set_size(len(lpaths))
        local = LocalFileSystem(auto_mkdir=True)
        if pre_stats is not None:
            stats = [pre_stats.get(r) for r in rpaths]
        else:
            # Classify directories with one stat_many batch.
            native_all = [self._native_path(self._strip_protocol(r)) for r in rpaths]
            stats, _stat_errors = self._client.stat_many(native_all)
        pairs = []
        for i, (remote_path, local_path) in enumerate(zip(rpaths, lpaths)):
            if stats[i] is not None and stats[i]["type"] == "directory":
                local.makedirs(local_path, exist_ok=True)
                callback.relative_update()
            else:
                pairs.append(
                    (
                        remote_path,
                        local_path,
                        stats[i].get("size") if stats[i] else 0,
                    )
                )
        if not pairs:
            return
        read_batch_bytes = min(self.max_batch_bytes, self.read_all_max_total_bytes)
        for batch in _bounded_batches(
            [size for _, _, size in pairs], self.batch_size, read_batch_bytes
        ):
            if len(batch) == 1 and pairs[batch[0]][2] > read_batch_bytes:
                remote_path, local_path, size = pairs[batch[0]]
                with callback.branched(remote_path, local_path) as child:
                    child.set_size(size)
                    local.makedirs(local._parent(local_path), exist_ok=True)
                    with open(local_path, "wb") as out:
                        with self.open(remote_path, "rb", cache_type="none") as remote:
                            while True:
                                chunk = remote.read(self._stream_read_size())
                                if not chunk:
                                    break
                                out.write(chunk)
                                child.relative_update(len(chunk))
                callback.relative_update()
                continue
            selected = [pairs[i] for i in batch]
            native = [
                self._native_path(self._strip_protocol(remote_path))
                for remote_path, _, _ in selected
            ]
            data, errors = self._client.read_all_many(native)
            for i, ((remote_path, local_path, _), buf) in enumerate(
                zip(selected, data)
            ):
                with callback.branched(remote_path, local_path) as child:
                    if buf is None:
                        raise _oserror(errors[i], remote_path)
                    local.makedirs(local._parent(local_path), exist_ok=True)
                    with open(local_path, "wb") as out:
                        child.set_size(len(buf))
                        out.write(buf)
                        child.relative_update(len(buf))
                callback.relative_update()

    def put(
        self,
        lpath,
        rpath,
        recursive=False,
        callback=DEFAULT_CALLBACK,
        maxdepth=None,
        **kwargs,
    ):
        """Copy local files to remote, writing every file in one writev batch
        instead of one round trip per file."""
        callback = Callback.as_callback(callback)
        from fsspec.implementations.local import (
            LocalFileSystem,
            make_path_posix,
            trailing_sep,
        )
        from fsspec.utils import other_paths

        if isinstance(lpath, list) and isinstance(rpath, list):
            rpaths = rpath
            lpaths = lpath
        else:
            source_is_str = isinstance(lpath, str)
            if source_is_str:
                lpath = make_path_posix(lpath)
            local = LocalFileSystem()
            lpaths = local.expand_path(
                lpath, recursive=recursive, maxdepth=maxdepth, **kwargs
            )
            if source_is_str and (not recursive or maxdepth is not None):
                lpaths = [p for p in lpaths if not (trailing_sep(p) or local.isdir(p))]
                if not lpaths:
                    return
            source_is_file = len(lpaths) == 1
            dest_is_dir = isinstance(rpath, str) and (
                trailing_sep(rpath) or self.isdir(rpath)
            )
            rpath = (
                self._strip_protocol(rpath)
                if isinstance(rpath, str)
                else [self._strip_protocol(p) for p in rpath]
            )
            exists = source_is_str and (
                (has_magic(lpath) and source_is_file)
                or (not has_magic(lpath) and dest_is_dir and not trailing_sep(lpath))
            )
            rpaths = other_paths(
                lpaths,
                rpath,
                exists=exists,
                flatten=not source_is_str,
            )

        callback.set_size(len(rpaths))
        pairs = []
        remote_dirs = []
        for local_path, remote_path in zip(lpaths, rpaths):
            if os.path.isdir(local_path):
                remote_dirs.append(remote_path)
                callback.relative_update()
            else:
                pairs.append((local_path, remote_path))
        if remote_dirs:
            self._makedirs_batched(remote_dirs, exist_ok=True)
        if not pairs:
            return
        mode = kwargs.get("mode", "overwrite")
        sizes = [os.path.getsize(local_path) for local_path, _ in pairs]
        for batch in _bounded_batches(sizes, self.batch_size, self.max_batch_bytes):
            if len(batch) == 1 and sizes[batch[0]] > self.max_batch_bytes:
                i = batch[0]
                local_path, remote_path = pairs[i]
                if mode == "create":
                    remote = self.open(remote_path, "xb")
                else:
                    remote = self.open(remote_path, "wb")
                with callback.branched(local_path, remote_path) as child:
                    child.set_size(sizes[i])
                    with remote, open(local_path, "rb") as source:
                        while True:
                            chunk = source.read(self.transfer_chunk_size)
                            if not chunk:
                                break
                            remote.write(chunk)
                            child.relative_update(len(chunk))
                callback.relative_update()
                continue
            selected = [pairs[i] for i in batch]
            datas = []
            for local_path, _ in selected:
                with open(local_path, "rb") as fh:
                    datas.append(fh.read())
            remote_paths = [
                self._strip_protocol(remote_path) for _, remote_path in selected
            ]
            self._write_batch(remote_paths, datas, mode=mode)
            for (local_path, remote_path), buf in zip(selected, datas):
                with callback.branched(local_path, remote_path) as child:
                    child.set_size(len(buf))
                    child.relative_update(len(buf))
                callback.relative_update()

    # -- open / file objects ----------------------------------------------

    def _open(
        self,
        path,
        mode="rb",
        block_size=None,
        autocommit=True,
        cache_type=None,
        cache_options=None,
        write_buffering=None,
        size=None,
        **kwargs,
    ):
        internal = self._strip_protocol(path)
        if not autocommit and any(c in mode for c in "wax"):
            # fsspec transactions: defer the write until commit()/discard().
            return _DeferredWriteFile(self, internal, mode)
        if self.auto_mkdir and any(c in mode for c in "wax"):
            parent = self._parent(path)
            if parent not in ("", "/"):
                self._ensure_dirs([parent], 0o755)
        base_mode = _normalize_mode(mode)
        if base_mode in ("r", "r+"):
            attrs = self._client.stat(self._native_path(internal))
            if attrs["type"] == "directory":
                raise IsADirectoryError(errno.EISDIR, "Is a directory", internal)
            if size is None:
                size = attrs["size"]
        return VfsiFile(
            self,
            internal,
            mode,
            block_size=self.block_size if block_size is None else block_size,
            cache_type=self.cache_type if cache_type is None else cache_type,
            cache_options=(
                self.cache_options if cache_options is None else cache_options
            ),
            write_buffering=(
                self.write_buffering
                if write_buffering is None
                else bool(write_buffering)
            ),
            size=size,
        )

    def open_many(
        self,
        open_files,
        *,
        block_size=None,
        cache_type=None,
        cache_options=None,
        write_buffering=None,
        sizes=None,
    ):
        """Open a list of ``OpenFile`` objects in one openv batch."""
        paths = [self._strip_protocol(f.path) for f in open_files]
        modes = [f.mode for f in open_files]
        effective_block_size = self.block_size if block_size is None else block_size
        effective_cache_type = self.cache_type if cache_type is None else cache_type
        effective_cache_options = (
            self.cache_options if cache_options is None else cache_options
        )
        effective_write_buffering = (
            self.write_buffering if write_buffering is None else bool(write_buffering)
        )
        if self.auto_mkdir and any(any(c in m for c in "wax") for m in modes):
            parents = {
                posixpath.dirname(p) for p in paths if posixpath.dirname(p) != "/"
            }
            self._ensure_dirs(list(parents), 0o755)
        mutating_paths = [
            path
            for path, mode in zip(paths, modes)
            if _normalize_mode(mode) in ("w", "w+", "a", "a+", "x", "x+")
        ]
        self._invalidate_namespace(mutating_paths)
        fds = self._client.open_many([self._native_path(p) for p in paths], modes)
        files = []
        try:
            if sizes is None:
                discovered_sizes = [None] * len(fds)
            else:
                discovered_sizes = list(sizes)
                if len(discovered_sizes) != len(fds):
                    raise ValueError("sizes must have one entry per open file")
            metadata_indices = [
                index
                for index, mode in enumerate(modes)
                if (
                    _normalize_mode(mode) == "r"
                    and effective_cache_type not in (None, "none")
                    and discovered_sizes[index] is None
                )
                or (
                    _normalize_mode(mode) == "a"
                    and effective_write_buffering
                    and discovered_sizes[index] is None
                )
            ]
            if metadata_indices:
                attrs = self._client.fstat_many(
                    [fds[index] for index in metadata_indices]
                )
                for index, attr in zip(metadata_indices, attrs):
                    discovered_sizes[index] = attr["size"]
            files = [
                VfsiFile(
                    self,
                    path,
                    mode,
                    fd=fd,
                    block_size=effective_block_size,
                    cache_type=effective_cache_type,
                    cache_options=effective_cache_options,
                    write_buffering=effective_write_buffering,
                    size=size,
                    defer_cache=True,
                )
                for path, mode, fd, size in zip(paths, modes, fds, discovered_sizes)
            ]
            _BufferGroup(self, files)
        except BaseException as setup_error:
            cleanup_error = None
            try:
                if fds and not self._client.closed:
                    self._client.close_many(fds)
            except BaseException as error:
                cleanup_error = error
                self._client.defer_close_many(fds)
            for file in files:
                file._finish_group_close()
            if cleanup_error is not None:
                raise setup_error from cleanup_error
            raise
        # Read-mode OpenFiles contexts do not call commit_many on exit;
        # register the opened files on their OpenFile objects so
        # OpenFile.__exit__ closes them (matching per-file opens on other
        # filesystems).
        for open_file, f in zip(open_files, files):
            if "r" in open_file.mode:
                open_file.fobjects = [f]
        return files

    def commit_many(self, open_files):
        """Flush and close a list of files in one closev batch."""
        group = open_files[0]._buffer_group if open_files else None
        if group is not None:
            error = None
            try:
                buffered = [file for file in open_files if file._buffered_write]
                for file in buffered:
                    file._stage_buffer()
                group.flush_files(buffered)
            except BaseException as exc:
                error = exc
            try:
                group.close_all()
            except BaseException:
                if error is None:
                    raise
            if error is not None:
                raise error
            return
        fds = [f._fd for f in open_files if not f.closed and f._fd is not None]
        if fds:
            self._client.close_many(fds)
        for f in open_files:
            f._fd = None
            f.close()

    # -- mutation ----------------------------------------------------------

    def mkdir(self, path, create_parents=True, **kwargs):
        internal = self._strip_protocol(path)
        mode = (kwargs.get("mode", 0o755) or 0o755) & 0o7777
        if create_parents:
            if self.exists(internal):
                raise FileExistsError(errno.EEXIST, "File exists", internal)
            self._ensure_dirs([internal], mode)
        else:
            self._invalidate_namespace([internal])
            self._client.mkdir(self._native_path(internal), mode)

    def makedirs(self, path, exist_ok=False):
        self._makedirs_batched([path], exist_ok)

    def rmdir(self, path):
        internal = self._strip_protocol(path)
        self._invalidate_namespace([internal])
        self._client.remove_many([self._native_path(internal)])

    def rm_file(self, path):
        """Remove one non-directory path using the native vector primitive."""
        self.rm(path, recursive=False)

    def rm(self, path, recursive=False, maxdepth=None):
        if isinstance(path, str):
            paths = [self._strip_protocol(path)]
        else:
            paths = [self._strip_protocol(p) for p in path]
        self._invalidate_namespace(paths)
        if recursive:
            self._rm_recursive(paths)
        else:
            # Like LocalFileSystem, non-recursive rm must not remove
            # directories (lstat so symlinks-to-directories are removed as
            # links, matching os.remove).
            stats, errors = self._client.lstat_many(
                [self._native_path(p) for p in paths]
            )
            for i, s in enumerate(stats):
                if s is not None and s["type"] == "directory":
                    raise ValueError("Cannot delete directory, set recursive=True")
            self._client.remove_many([self._native_path(p) for p in paths])

    def _rm_recursive(self, paths):
        """Recursively remove ``paths`` with a batched walk + removev."""
        native_paths = [self._native_path(p) for p in paths]
        lstats, lerrs = self._client.lstat_many(native_paths)
        if lerrs:
            i = min(lerrs)
            raise _oserror(lerrs[i], paths[i])

        remove_files = []
        remove_dirs = []
        for i, p in enumerate(paths):
            if lstats[i] is None:
                continue
            if lstats[i]["type"] != "directory":
                remove_files.append(native_paths[i])
                continue
            remove_dirs.append(native_paths[i])
            tree = self._client.walk(native_paths[i])
            for _, entries in tree:
                for e in entries:
                    if e["type"] == "directory":
                        remove_dirs.append(e["name"])
                    else:
                        remove_files.append(e["name"])

        if remove_files:
            self._client.remove_many(list(dict.fromkeys(remove_files)))
        remove_dirs = list(dict.fromkeys(remove_dirs))
        remove_dirs.sort(key=lambda p: p.count("/"), reverse=True)
        if remove_dirs:
            self._client.remove_many(remove_dirs)

    def mv(self, path1, path2, recursive=False, maxdepth=None, **kwargs):
        if isinstance(path1, list) or isinstance(path2, list):
            if not (
                isinstance(path1, list)
                and isinstance(path2, list)
                and len(path1) == len(path2)
            ):
                raise ValueError("path1 and path2 must both be lists of equal length")
            pairs = [
                (self._strip_protocol(a), self._strip_protocol(b))
                for a, b in zip(path1, path2)
            ]
        else:
            src = self._strip_protocol(path1)
            dst = self._strip_protocol(path2)
            if src == dst:
                return
            if self.isdir(dst):
                # Like shutil.move / base copy+rm: moving onto an existing
                # directory moves the source inside it.
                dst = posixpath.join(dst, posixpath.basename(src))
            pairs = [(src, dst)]
        self._invalidate_namespace([path for pair in pairs for path in pair])
        self._client.rename_many(
            [(self._native_path(a), self._native_path(b)) for a, b in pairs]
        )

    def _copy_recursive(self, src, dst, symlinks=False, callback=DEFAULT_CALLBACK):
        """Copy a directory tree with batched walk/mkdir/copy calls."""
        native_src = self._native_path(src)
        src = src.rstrip("/") or "/"
        dst = dst.rstrip("/") or "/"
        tree = self._client.walk(native_src)

        dest_dirs = {dst}
        pairs = []
        symlink_pairs = []
        for dir_native, entries in tree:
            dir_int = self._internalize(dir_native)
            rel = posixpath.relpath(dir_int, src)
            dest_dir = posixpath.join(dst, rel) if rel != "." else dst
            dest_dirs.add(dest_dir)
            for e in entries:
                child_int = self._internalize(e["name"])
                rel_child = posixpath.relpath(child_int, src)
                dest = posixpath.join(dst, rel_child) if rel_child != "." else dst
                if e["type"] == "directory":
                    dest_dirs.add(dest)
                elif e["type"] == "symlink" and symlinks:
                    symlink_pairs.append((child_int, dest))
                else:
                    pairs.append((child_int, dest))

        callback.set_size(len(pairs) + len(symlink_pairs))
        self._ensure_dirs(list(dest_dirs), 0o755)
        self._copy_pairs(
            pairs,
            "raise",
            callback=callback,
            callback_is_parent=True,
            set_callback_size=False,
        )
        for s, d in symlink_pairs:
            target = self.readlink(s)
            self.symlink(target, d)
            _complete_child(callback, s, d, 0)

    def cp_file(self, path1, path2, callback=DEFAULT_CALLBACK, **kwargs):
        """Copy a single file (or create a directory) between two paths."""
        callback = Callback.as_callback(callback)
        src = self._strip_protocol(path1)
        dst = self._strip_protocol(path2)
        info = self.info(src)
        if info["type"] == "directory":
            self._ensure_dirs([dst], 0o755)
            _complete_callback(callback, 0)
            return
        if info["type"] != "file":
            raise FileNotFoundError(src)
        parent = self._parent(dst)
        if self.auto_mkdir and parent not in ("", "/"):
            self._ensure_dirs([parent], 0o755)
        self._copy_pairs([(src, dst)], "raise", callback=callback)

    def copy(
        self,
        path1,
        path2,
        recursive=False,
        maxdepth=None,
        on_error=None,
        callback=DEFAULT_CALLBACK,
        **kwargs,
    ):
        callback = Callback.as_callback(callback)
        if on_error is None:
            on_error = "ignore" if recursive else "raise"
        if isinstance(path1, list) and isinstance(path2, list):
            if len(path1) != len(path2):
                raise ValueError("path1 and path2 must be lists of equal length")
            pairs = [
                (self._strip_protocol(a), self._strip_protocol(b))
                for a, b in zip(path1, path2)
            ]
            self._copy_pairs(
                pairs, on_error, callback=callback, callback_is_parent=True
            )
            return
        # Batched tree copy for the plain "directory -> new name" case.
        if (
            isinstance(path1, str)
            and not has_magic(path1)
            and recursive
            and maxdepth is None
            and not path1.endswith("/")
            and not (isinstance(path2, str) and path2.endswith("/"))
        ):
            src = self._strip_protocol(path1)
            dst = self._strip_protocol(path2)
            stats, errors = self._client.stat_many(
                [self._native_path(src), self._native_path(dst)]
            )
            src_is_dir = stats[0] is not None and stats[0]["type"] == "directory"
            dst_is_dir = stats[1] is not None and stats[1]["type"] == "directory"
            if errors:
                i = min(errors)
                if errors[i] != errno.ENOENT:
                    raise _oserror(errors[i], (src, dst)[i])
            if src_is_dir and not dst_is_dir:
                self._copy_recursive(
                    src,
                    dst,
                    kwargs.get("symlinks", False),
                    callback=callback,
                )
                return
        # Resolve globs and trailing-slash/maxdepth cases like fsspec's base
        # implementation, then retain vectorized stat/copy operations and a
        # single parent callback for the complete transfer.
        from fsspec.implementations.local import trailing_sep
        from fsspec.utils import other_paths

        source_is_str = isinstance(path1, str)
        paths1 = self.expand_path(
            path1, recursive=recursive, maxdepth=maxdepth, **kwargs
        )
        if source_is_str and (not recursive or maxdepth is not None):
            paths1 = [p for p in paths1 if not (trailing_sep(p) or self.isdir(p))]
            if not paths1:
                callback.set_size(0)
                return
        source_is_file = len(paths1) == 1
        dest_is_dir = isinstance(path2, str) and (
            trailing_sep(path2) or self.isdir(path2)
        )
        exists = source_is_str and (
            (has_magic(path1) and source_is_file)
            or (not has_magic(path1) and dest_is_dir and not trailing_sep(path1))
        )
        paths2 = other_paths(paths1, path2, exists=exists, flatten=not source_is_str)
        pairs = [
            (self._strip_protocol(a), self._strip_protocol(b))
            for a, b in zip(paths1, paths2)
        ]
        callback.set_size(len(pairs))
        stats, errors = self._client.stat_many(
            [self._native_path(src) for src, _ in pairs]
        )
        file_pairs = []
        dir_pairs = []
        first_error = None
        for i, ((src, dst), stat) in enumerate(zip(pairs, stats)):
            if i in errors:
                callback.relative_update()
                if on_error == "raise" and first_error is None:
                    first_error = _oserror(errors[i], src)
            elif stat is not None and stat["type"] == "directory":
                dir_pairs.append((src, dst))
            elif stat is not None and stat["type"] == "file":
                file_pairs.append((src, dst))
            else:
                callback.relative_update()
                if first_error is None:
                    first_error = FileNotFoundError(src)
        if dir_pairs:
            self._ensure_dirs([dst for _, dst in dir_pairs], 0o755)
            for src, dst in dir_pairs:
                _complete_child(callback, src, dst, 0)
        self._copy_pairs(
            file_pairs,
            on_error,
            callback=callback,
            callback_is_parent=True,
            set_callback_size=False,
        )
        if first_error is not None:
            raise first_error

    cp = copy

    def _copy_pairs(
        self,
        pairs,
        on_error,
        callback=None,
        callback_is_parent=False,
        set_callback_size=True,
    ):
        if callback is not None and set_callback_size and callback_is_parent:
            callback.set_size(len(pairs))
        if not pairs:
            return
        self._invalidate_namespace([dst for _, dst in pairs])
        native_pairs = [(self._native_path(a), self._native_path(b)) for a, b in pairs]
        copied, errors = self._client.copy_many(native_pairs)
        if errors and all(err == 2 for err in errors.values()):
            if self.auto_mkdir:
                # Missing destination parents: create them once, then retry.
                parents = {posixpath.dirname(b) for _, b in native_pairs}
                internal_parents = [
                    self._internalize(parent)
                    for parent in sorted(parents)
                    if parent not in ("", "/")
                ]
                self._ensure_dirs(internal_parents, 0o755)
                copied, errors = self._client.copy_many(native_pairs)
        if callback is not None:
            for i, (src, dst) in enumerate(pairs):
                if i in errors or copied[i] is None:
                    if callback_is_parent:
                        callback.relative_update()
                    continue
                if callback_is_parent:
                    _complete_child(callback, src, dst, copied[i])
                else:
                    _complete_callback(callback, copied[i])
        if errors:
            i = min(errors)
            if errors[i] == _ERR_SAME_FILE:
                src, dst = pairs[i]
                exc = shutil.SameFileError(f"{src!r} and {dst!r} are the same file")
            else:
                exc = _oserror(errors[i], pairs[i][0])
            if on_error == "raise":
                raise exc

    def touch(self, path, truncate=True, **kwargs):
        internal = self._strip_protocol(path)
        if not truncate and self.exists(internal):
            self._invalidate_namespace([internal])
            self._client.touch(self._native_path(internal))
            return
        self._write_batch([internal], [b""])

    def symlink(self, target, path, **kwargs):
        self._invalidate_namespace([path])
        link = self._native_path(self._strip_protocol(path))
        protocols = (
            (type(self).protocol,)
            if isinstance(type(self).protocol, str)
            else type(self).protocol
        )
        absolute_target = target.startswith("/") or any(
            target.startswith((protocol + "://", protocol + "::"))
            for protocol in protocols
        )
        if absolute_target:
            # Absolute targets are relative to the filesystem root (chroot
            # semantics, matching LocalFileSystem's OS-absolute targets);
            # map them through the root prefix. Relative targets are stored
            # as-is and resolve relative to the link's directory.
            target = self._native_path(self._strip_protocol(target))
        self._client.symlink(target, link)

    def readlink(self, path):
        return self._client.readlink(self._native_path(self._strip_protocol(path)))

    def hardlink(self, src, dst):
        self._invalidate_namespace([dst])
        self._invalidate_parent_listing(src)
        self._client.hardlink(
            self._native_path(self._strip_protocol(src)),
            self._native_path(self._strip_protocol(dst)),
        )

    # -- traversal ---------------------------------------------------------

    def walk(self, path, maxdepth=None, topdown=True, on_error="omit", **kwargs):
        if maxdepth is not None and maxdepth < 1:
            raise ValueError("maxdepth must be at least 1")
        detail = kwargs.pop("detail", False)
        refresh = kwargs.pop("refresh", False)
        internal = self._strip_protocol(path)
        tree = None if refresh else self._cached_walk_tree(internal, maxdepth)
        if tree is None:
            # A native miss refreshes the complete subtree in one vectorized
            # walk and removes cached descendants no longer present remotely.
            self.invalidate_cache(internal)
            fill_epoch = self._dircache_epoch_snapshot()
            try:
                tree = self._native_walk_tree(internal, fill_epoch, maxdepth=maxdepth)
            except (FileNotFoundError, OSError) as e:
                # Resource-limit failures mean the materialized result would
                # be incomplete. Never turn them into an apparently empty
                # tree, even when ordinary traversal errors are omitted.
                if getattr(e, "errno", None) == errno.EFBIG:
                    raise
                if self.isfile(internal):
                    info = self.info(internal)
                    files = {"": info} if detail else [""]
                    yield internal, [], files
                    return
                if on_error == "raise":
                    raise
                if callable(on_error):
                    on_error(e)
                return
        by_dir = {}
        for directory, infos in tree:
            dirs = {}
            files = {}
            for info in infos:
                entry_internal = self._strip_protocol(info["name"])
                name = posixpath.basename(entry_internal.rstrip("/"))
                (dirs if info["type"] == "directory" else files)[name] = dict(info)
            by_dir[directory] = (dirs, files)
        root_depth = _depth(internal)
        order = [
            d for d in by_dir if maxdepth is None or _depth(d) <= root_depth + maxdepth
        ]
        if not topdown:
            order = list(reversed(order))
        for d in order:
            dirs, files = by_dir[d]
            if not detail:
                dirs = list(dirs)
                files = list(files)
            yield d, dirs, files

    def find(self, path, maxdepth=None, withdirs=False, detail=False, **kwargs):
        internal = self._strip_protocol(path)
        root_depth = _depth(internal)
        out = {}
        if withdirs and internal != "" and self.isdir(internal):
            out[internal] = self.info(internal)
        for _, dirs, files in self.walk(
            internal, maxdepth=maxdepth, detail=True, **kwargs
        ):
            entries = list(files.values())
            if withdirs:
                entries.extend(dirs.values() if isinstance(dirs, dict) else [])
            for info in entries:
                full = self._strip_protocol(info["name"])
                if maxdepth is not None and _depth(full) > root_depth + maxdepth:
                    continue
                out[full] = dict(info)
        if not out and self.isfile(internal):
            out[internal] = self.info(internal) if withdirs else {}
        names = sorted(out)
        if not detail:
            return names
        return {n: out[n] for n in names}

    def du(self, path, total=True, maxdepth=None, withdirs=False, **kwargs):
        internal = self._strip_protocol(path)
        sizes = {}
        if withdirs and self.isdir(internal):
            info = self.info(internal)
            sizes[info["name"]] = info.get("size") or 0
        for _, dirs, files in self.walk(
            internal, maxdepth=maxdepth, detail=True, **kwargs
        ):
            for info in files.values():
                sizes[info["name"]] = info.get("size") or 0
            if withdirs:
                for info in dirs.values():
                    sizes[info["name"]] = info.get("size") or 0
        if not sizes and self.isfile(internal):
            info = self.info(internal)
            sizes[info["name"]] = info.get("size") or 0
        if total:
            return sum(sizes.values())
        return sizes
