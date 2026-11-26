"""fsspec filesystem implementation for the vectorized VFSI backends."""

import datetime
import errno
import hashlib
import inspect
import io
import math
import operator
import os
import posixpath
import shutil
import tempfile
import threading
import uuid
import weakref
from collections import OrderedDict, deque
from collections.abc import MutableMapping
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import ExitStack, contextmanager
from contextvars import ContextVar
from copy import copy
from functools import partial
from glob import has_magic
from itertools import groupby
from urllib.parse import unquote, urlsplit

from fsspec.caching import AllBytes, BlockCache, KnownPartsOfAFile, caches
from fsspec.callbacks import DEFAULT_CALLBACK, Callback
from fsspec.compression import compr
from fsspec.core import PickleableTextIOWrapper, get_compression
from fsspec.spec import AbstractBufferedFile, AbstractFileSystem
from fsspec.transaction import Transaction

from ._caching import _Nfs4MMapCache

__all__ = ["VfsiFile", "VfsiFileSystem"]

_MEMORY_PATH = "<memory>"
_OPERATION_WRITERS = ContextVar("vfsi_operation_writers", default=None)
_SCALAR_CACHE_SETUP = ContextVar("vfsi_scalar_cache_setup", default=None)
_ERR_SAME_FILE = 0xFFFF_FFFD
_ERR_UNSUPPORTED = 0xFFFF_FFFE
_NFS_RETRY_UNCACHED_REP = 10068


def _path_string(path):
    """Coerce scalar paths without losing separators or changing collections."""
    return os.fsdecode(path) if isinstance(path, os.PathLike) else path


def _normalize_mode(mode):
    """Collapse 'b'/'t' characters: 'rt'/'r'/'rb' -> 'r', 'wb+' -> 'w+', ..."""
    if not isinstance(mode, str):
        raise TypeError("mode must be a string")
    base = mode.replace("b", "").replace("t", "")
    if mode.count("b") + mode.count("t") > 1 or base not in (
        "r",
        "r+",
        "w",
        "w+",
        "a",
        "a+",
        "x",
        "x+",
    ):
        raise ValueError(f"unsupported mode: {mode!r}")
    return base


def _writable_byte_view(buffer):
    """Validate a readinto destination before any data is consumed."""
    view = memoryview(buffer)
    if view.readonly:
        raise TypeError("readinto() requires a writable buffer")
    return view.cast("B")


def _is_transport_error(error):
    return (
        isinstance(error, (ConnectionError, TimeoutError))
        or getattr(error, "transport_kind", None) is not None
    )


def _is_uncertain_mutation(error):
    # The original operation may have applied even though the reply could
    # not be replayed from the NFS session cache.
    return (
        _is_transport_error(error)
        or getattr(error, "failure_state", None) == "unknown"
        or (
            getattr(error, "status_domain", None) == "nfs"
            and getattr(error, "status_code", None) == _NFS_RETRY_UNCACHED_REP
        )
    )


def _can_reconnect(error):
    return _is_transport_error(error) and getattr(
        error, "transport_kind", None
    ) not in ("auth", "invalid_reply")


def _errno(error):
    """Read errno from structured failures or legacy/mock integer results."""
    return getattr(error, "errno", error)


def _oserror(errno_code, path, *, index=None):
    """Preserve structured native exceptions; accept legacy/mock errno maps."""
    if isinstance(errno_code, BaseException):
        error = errno_code
    elif errno_code == _ERR_UNSUPPORTED:
        error = NotImplementedError(f"operation is unsupported: {path!r}")
    else:
        # OSError selects the appropriate subclass from an actual OS errno.
        try:
            message = os.strerror(errno_code)
        except (OverflowError, ValueError):
            message = f"errno {errno_code}"
        error = OSError(errno_code, message, path)
    if isinstance(error, OSError) and error.filename is None:
        error.filename = path
    # Native vectors can be subsets of a public request. Their callers supply
    # the public position here, without losing the native status/diagnostics.
    if index is not None:
        error.index = index
    return error


def _rebase_error(error, indices, paths):
    """Map an attributed vector error through the selected public inputs."""
    position = getattr(error, "index", None)
    if (
        isinstance(position, int)
        and not isinstance(position, bool)
        and 0 <= position < len(indices)
    ):
        _oserror(error, paths[position], index=indices[position])


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
        "fs_id": attrs.get("fs_id"),
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


def _timestamp_datetime(info, attribute):
    """Return an aware UTC datetime using available nanosecond metadata."""
    nanoseconds = info.get(f"{attribute}_ns")
    if nanoseconds is not None:
        seconds, fraction = divmod(int(nanoseconds), 1_000_000_000)
        value = datetime.datetime.fromtimestamp(seconds, tz=datetime.timezone.utc)
        return value + datetime.timedelta(microseconds=fraction // 1_000)
    seconds = info.get(attribute)
    if seconds is None:
        return None
    return datetime.datetime.fromtimestamp(seconds, tz=datetime.timezone.utc)


def _depth(path):
    """Number of components below the root for an internal path."""
    if path == "/":
        return 0
    return path.count("/")


class _WalkBudget:
    """One traversal's remaining materialized-entry and native-path budgets."""

    def __init__(self, fs, root):
        self.fs = fs
        self.root = root
        self.entries = fs.directory_max_entries
        self.path_bytes = fs.directory_max_path_bytes
        self.prefetched = {}
        self.epoch = fs._dircache_epoch_snapshot()

    def native_options(self):
        return {"max_entries": self.entries, "max_path_bytes": self.path_bytes}

    def sync(self):
        epoch = self.fs._dircache_epoch_snapshot()
        if epoch != self.epoch:
            self.release_prefetched()
            self.epoch = epoch

    def _path_cost(self, infos):
        return sum(
            len(
                os.fsencode(
                    self.fs._native_path(self.fs._checked_strip_protocol(info["name"]))
                )
            )
            for info in infos
        )

    def release_prefetched(self):
        if not self.prefetched:
            return False
        for infos in self.prefetched.values():
            self.entries += len(infos)
            self.path_bytes += self._path_cost(infos)
        self.prefetched.clear()
        return True

    def account(self, infos, *, prefetch=False):
        entries = len(infos)
        path_bytes = self._path_cost(infos)
        if entries > self.entries or path_bytes > self.path_bytes:
            if prefetch:
                return False
            # Prefer the current visit over reservations for later siblings.
            self.release_prefetched()
            if entries > self.entries or path_bytes > self.path_bytes:
                raise _oserror(errno.EFBIG, self.fs._fullpath(self.root))
        self.entries -= entries
        self.path_bytes -= path_bytes
        return True

    def listdir(self, native_path):
        try:
            return self.fs._client.listdir(native_path, **self.native_options())
        except OSError as error:
            if error.errno != errno.EFBIG or not self.release_prefetched():
                raise
        return self.fs._client.listdir(native_path, **self.native_options())


def _require_size(attrs, path, *, index=None):
    size = attrs.get("size") if attrs is not None else None
    if size is None:
        raise _oserror(
            OSError(errno.EIO, "file size metadata is unavailable", path),
            path,
            index=index,
        )
    return size


def _range_file_size(attrs, path):
    if attrs is not None and attrs.get("type") == "directory":
        raise IsADirectoryError(errno.EISDIR, "Is a directory", path)
    return _require_size(attrs, path)


def _range_indices(start, end):
    return (
        0 if start is None else operator.index(start),
        None if end is None else operator.index(end),
    )


def _normalize_range(start, end, size=None):
    """Normalize fsspec byte-range bounds for the native unsigned offsets."""
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


def _check_range_offsets(start, end):
    if start >= 1 << 64 or end >= 1 << 64:
        raise OverflowError("byte range exceeds unsigned 64-bit offsets")


def _validate_write_mode(mode):
    if mode not in ("overwrite", "create"):
        raise ValueError("mode must be 'overwrite' or 'create'")


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


def _require_empty_read(remote, path, limit, allocation_base=0):
    # Use the pinned size when available, without allocating a data buffer.
    size = getattr(remote, "size", None)
    if size is None:
        size = len(remote.read(1))
    if size:
        raise _allocation_error(path, allocation_base + 1, limit)


def _handle_identity(attrs):
    """Object identity within this filesystem, not a file-version token.

    Native ``created`` fields currently contain POSIX ctime, which changes on
    writes and chmod. Never use them to identify an inode across reconnects.
    IDs are scoped to the configured backend/server. Both filesystem and
    file ID are required; missing fields fail closed. Zero is a valid ID.
    """
    fs_id, file_id = attrs.get("fs_id"), attrs.get("fileid")
    return None if fs_id is None or file_id is None else (fs_id, file_id)


def _stale_handle_error(path):
    return OSError(errno.ESTALE, "cannot safely restore the original file handle", path)


def _close_reader_on_exit(reader, exc_type, primary_error, traceback, *, close=None):
    """Preserve file failures and retain native cleanup ownership on exit."""
    raw = getattr(reader, "_raw", reader)
    fd = getattr(raw, "_fd", None)
    try:
        (reader.close if close is None else close)()
    except BaseException as cleanup_error:
        if fd is not None:
            group = getattr(reader, "_buffer_group", None)
            tokens = [fd]
            if group is not None:
                with group._lock:
                    # A failed member exit is still a close request. Its token
                    # can be deferred without abandoning healthy open siblings.
                    for member in group.files:
                        if member._raw is raw:
                            group._close_requested.add(id(member))
                            break
                    if len(group._close_requested) == len(group.files):
                        tokens = [member._raw._fd for member in group.files]
            raw.fs._client.defer_close_many(tokens)
        if primary_error is not None:
            raise primary_error from cleanup_error
        raise


def _finish_group_files(files):
    """Release every member's local resources, retaining the first failure."""
    first_error = None
    for file in files:
        try:
            file._finish_group_close()
        except BaseException as error:
            first_error = first_error or error
    return first_error


def _enter_individually(open_files):
    """Enter wrappers with primary-error and descriptor ownership preserved."""
    entered = []
    try:
        for open_file in open_files:
            entered.append(open_file.__enter__())
    except BaseException as primary_error:
        cleanup_error = None
        # The failing entry may already own a handle before wrapper setup fails.
        for open_file in reversed(open_files[: len(entered) + 1]):
            try:
                open_file.__exit__(
                    type(primary_error), primary_error, primary_error.__traceback__
                )
            except BaseException as error:
                cleanup_error = cleanup_error or error
                for stream in open_file.fobjects:
                    raw = getattr(stream, "_raw", stream)
                    if isinstance(raw, _RawVfsiFile):
                        raw.fs._client.defer_close_many([raw._fd])
        if cleanup_error is not None:
            raise primary_error from cleanup_error
        raise
    return entered


def _write_fully(file, data, on_progress=None, *, none_is_full=False):
    """Finish a chunk without treating a valid short write as completion."""
    offset = 0
    while offset < len(data):
        written = file.write(data[offset:])
        if written is None and none_is_full:
            written = len(data) - offset
        if written <= 0 or written > len(data) - offset:
            raise OSError(errno.EIO, "invalid progress during streamed write")
        offset += written
        if on_progress is not None:
            on_progress(written)


class _TextIOWrapper(PickleableTextIOWrapper):
    """Preserve application failures when text-stream cleanup also fails."""

    def __exit__(self, *args):
        _close_reader_on_exit(self, *args)

    def __reduce__(self):
        return type(self), self.args


class _OwnedStream(io.IOBase):
    """A codec stream whose close also releases its explicitly owned file."""

    def __init__(self, stream, owner, *, fs=None, path=None):
        self._stream = stream
        self._owner = owner
        self._raw = getattr(owner, "_raw", owner)
        self._policy_fs = fs if fs is not None else getattr(self._raw, "fs", None)
        self._read_path = getattr(self._raw, "path", path)
        self._closed = False

    def _check_open(self):
        if self._closed:
            raise ValueError("I/O operation on closed file")

    def __getattr__(self, name):
        attribute = getattr(self._stream, name)
        if not callable(attribute):
            return attribute

        def call(*args, **kwargs):
            self._check_open()
            return attribute(*args, **kwargs)

        return call

    @property
    def closed(self):
        return self._closed

    def close(self):
        if self._closed:
            return
        # Finalization can emit bytes. Never repeat it after an ambiguous
        # error; the underlying file's cleanup helper owns any deferred CLOSE.
        self._closed = True
        with ExitStack() as stack:
            stack.push(partial(_close_reader_on_exit, self._owner))
            self._stream.close()

    def __enter__(self):
        self._check_open()
        return self

    def __exit__(self, *args):
        _close_reader_on_exit(self, *args)

    def __iter__(self):
        self._check_open()
        return self

    def __next__(self):
        line = self.readline()
        if not line:
            raise StopIteration
        return line

    def readable(self):
        self._check_open()
        return self._stream.readable()

    def writable(self):
        self._check_open()
        return self._stream.writable()

    def seekable(self):
        self._check_open()
        return self._stream.seekable()

    def seek(self, offset, whence=0):
        self._check_open()
        return self._stream.seek(offset, whence)

    def tell(self):
        self._check_open()
        return self._stream.tell()

    def truncate(self, size=None):
        self._check_open()
        return self._stream.truncate(size)

    def flush(self):
        self._check_open()
        return self._stream.flush()

    def fileno(self):
        self._check_open()
        return self._stream.fileno()

    def _read_bounded(self, method, size, *, advances=True):
        self._check_open()
        size = -1 if size is None else operator.index(size)
        fs = self._policy_fs
        if fs is None:
            return method(size)
        limit = fs.read_all_max_total_bytes
        # A one-byte sentinel distinguishes EOF at the cap from an oversized
        # decoded result. Never ask a codec to materialize unbounded output.
        bounded = limit + 1 if size < 0 else min(size, limit + 1)
        data = method(bounded)
        try:
            fs._check_read_allocation(len(data), self._read_path)
        except OSError as error:
            if advances:
                self._rewind_rejected_read(len(data), error)
            raise
        return data

    def _rewind_rejected_read(self, consumed, error):
        try:
            if self._stream.seekable():
                self._stream.seek(self._stream.tell() - consumed)
        except Exception as rewind_error:
            raise error from rewind_error

    def read(self, size=-1):
        return self._read_bounded(self._stream.read, size)

    def read1(self, size=-1):
        method = getattr(self._stream, "read1", self._stream.read)
        return self._read_bounded(method, size)

    def peek(self, size=0):
        # Codecs treat size as a hint and may return more; check actual bytes.
        return self._read_bounded(
            self._stream.peek, operator.index(size), advances=False
        )

    def readline(self, size=-1):
        return self._read_bounded(self._stream.readline, size)

    def readlines(self, hint=-1):
        self._check_open()
        fs = self._policy_fs
        if fs is None:
            return io.IOBase.readlines(self, hint)
        hint = -1 if hint is None else operator.index(hint)
        limit = fs.read_all_max_total_bytes
        lines = []
        consumed = 0
        while True:
            # Read one byte past the remaining budget to distinguish EOF from
            # a line that would put the returned list over the limit.
            line = self._stream.readline(limit - consumed + 1)
            if not line:
                return lines
            consumed += len(line)
            if consumed > limit:
                error = _allocation_error(self._read_path, consumed, limit)
                self._rewind_rejected_read(consumed, error)
                raise error
            lines.append(line)
            if hint > 0 and consumed > hint:
                return lines


def _validate_text_options(options):
    # Use the same stdlib validation as the actual wrapper, without opening
    # a file that a write mode could create or truncate.
    with io.TextIOWrapper(io.BytesIO(), **options):
        pass


def _open_with_wrappers(fs, opener, path, mode="rb", compression=None, **kwargs):
    """Construct text/codecs with one owner and shared failure cleanup."""
    base_mode = _normalize_mode(mode)
    text = "b" not in mode
    text_kwargs = {
        key: kwargs.pop(key)
        for key in ("encoding", "errors", "newline")
        if text and key in kwargs
    }
    binary_mode = mode.replace("t", "") + "b" if text else mode
    codec = get_compression(path, compression) if compression is not None else None
    if text_kwargs and base_mode != "r":
        _validate_text_options(text_kwargs)
    file = None
    try:
        with ExitStack() as stack:
            file = opener(path, binary_mode, **kwargs)
            stack.push(partial(_close_reader_on_exit, file))
            stream = file
            if codec is not None:
                stream = _OwnedStream(
                    compr[codec](file, mode=binary_mode[0]), file, fs=fs, path=path
                )
                stack.push(partial(_close_reader_on_exit, stream))
            if text:
                stream = _TextIOWrapper(stream, **text_kwargs)
            stack.pop_all()
            return stream
    except BaseException as primary:
        if isinstance(file, _DeferredWriteFile):
            transaction = fs._transaction
            if transaction is not None and file in transaction.files:
                transaction.files.remove(file)
            try:
                file.discard()
            except BaseException as cleanup:
                raise primary from cleanup
        raise


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
            self.reconnect()

    def reconnect(self):
        old = self._native
        pid = os.getpid()
        inherited = self._pid != pid
        self._native = self._native_module.NfsClient(*self._factory_args)
        self._pid = pid
        self._generation += 1
        if old is not None:
            if inherited:
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
            if self._pid != os.getpid():
                # A child must not CLOSE handles or destroy its parent's session.
                old._abandon_after_fork()
            else:
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
            except OSError as error:
                if (
                    not _can_reconnect(error)
                    or not self._auto_reconnect
                    or name not in self._IDEMPOTENT
                ):
                    raise
                self.reconnect()
                return getattr(self._native, name)(*args, **kwargs)

        return call


_FORK_POOLS = weakref.WeakSet()


def _reset_pool_locks_after_fork():
    for pool in _FORK_POOLS:
        pool._reset_locks_after_fork()


if hasattr(os, "register_at_fork"):
    # The interpreter retains registered hooks for its lifetime. Keep one
    # hook and weak ownership so retired filesystems leave no callbacks.
    os.register_at_fork(after_in_child=_reset_pool_locks_after_fork)


class _RetiredDescriptorError(OSError):
    """A token was rejected by the pool before any native call."""


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
            "ftruncate",
        }
    )
    _FD_MANY_METHODS = frozenset(
        {
            "fstat_many",
            "pread_many",
            "pwrite_many",
            "append_many",
            "close_many",
            "preadv",
            "pwritev",
            "appendv",
        }
    )

    def __init__(self, native_module, factory_args, size=1, auto_reconnect=True):
        self._clients = [
            _ResilientClient(native_module, factory_args, auto_reconnect=False)
            for _ in range(size)
        ]
        self._auto_reconnect = auto_reconnect
        self._native_has_vector_outcomes = all(
            hasattr(native_module.NfsClient, name)
            for name in ("preadv", "pwritev", "appendv")
        )
        try:
            self._native_has_deferred_close = (
                "_deferred"
                in inspect.signature(native_module.NfsClient.close_many).parameters
            )
        except (AttributeError, TypeError, ValueError):
            self._native_has_deferred_close = False
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
            _FORK_POOLS.add(self)

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
            if owner >= len(self._clients):
                raise ValueError("filesystem is closed")
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
                raise _RetiredDescriptorError(
                    errno.EBADF, "Bad file descriptor"
                ) from None

    def _resolve_many(self, tokens):
        resolved = []
        for index, token in enumerate(tokens):
            try:
                resolved.append(self._resolve(token))
            except _RetiredDescriptorError as error:
                error.index = index
                raise
        return resolved

    def descriptor_valid(self, token):
        """Return whether a virtual descriptor still belongs to a live session."""
        with self._lock:
            return token in self._fds

    def defer_close_many(self, tokens):
        """Retain cleanup ownership when setup fails and CLOSE is unavailable."""
        with self._lock:
            self._deferred_close.update(token for token in tokens if token in self._fds)

    def _tokens_by_owner(self, tokens):
        with self._lock:
            by_owner = {}
            for token in dict.fromkeys(tokens):
                mapping = self._fds.get(token)
                if mapping is not None:
                    by_owner.setdefault(mapping[0], []).append(token)
            return by_owner

    def _close_live_token(self, token):
        for owner, tokens in self._tokens_by_owner([token]).items():
            self._close_live_tokens(owner, tokens, scalar=True)

    def _close_live_tokens(self, owner, tokens, *, scalar=False):
        if not self.closed and self._pid != os.getpid():
            self.ensure_ready()
        # Revalidate after taking the same lock used for session retirement.
        # Keep it through CLOSE so the vector cannot acquire stale tokens.
        with self._client_locks[owner]:
            live = self._tokens_by_owner(tokens).get(owner)
            if live:
                try:
                    if scalar:
                        self.close(live[0])
                    else:
                        self.close_many(live)
                except (OSError, ValueError) as error:
                    if not self.closed or (
                        isinstance(error, OSError) and error.errno != errno.EBADF
                    ):
                        raise

    def _close_setup_handles(self, tokens, batch_size):
        first_error = None
        for owner, owned in self._tokens_by_owner(tokens).items():
            for start in range(0, len(owned), batch_size):
                batch = owned[start : start + batch_size]
                try:
                    self._close_live_tokens(owner, batch)
                except BaseException as error:
                    first_error = first_error or error
                    self.defer_close_many(batch)
        return first_error

    def _discard_tokens(self, tokens):
        with self._lock:
            for token in tokens:
                self._fds.pop(token, None)
                self._deferred_close.discard(token)

    def _discard_confirmed_closes(self, error, tokens):
        # An error index identifies one failed input, never a successful prefix.
        indices = getattr(error, "_closed_indices", None)
        if isinstance(indices, (list, tuple)) and all(
            type(index) is int and 0 <= index < len(tokens) for index in indices
        ):
            self._discard_tokens([tokens[index] for index in indices])

    def _close_deferred_many(self, client, fds):
        # Older native modules lack ownership reconciliation; keep their
        # ordinary cleanup signature usable when the shared Python layer updates.
        if self._native_has_deferred_close:
            return client.close_many(fds, _deferred=True)
        return client.close_many(fds)

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
                    if not live:
                        continue
                    client = self._clients[owner]
                try:
                    self._close_deferred_many(
                        client, [native_fd for _, native_fd in live]
                    )
                except BaseException as error:
                    self._discard_confirmed_closes(error, [token for token, _ in live])
                    if not isinstance(error, Exception):
                        raise
                    if _is_transport_error(error):
                        if self._auto_reconnect and _can_reconnect(error):
                            try:
                                self._reconnect_client(owner)
                            except Exception:
                                pass
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

    def _reconnect_client(self, owner):
        """Replace one failed session without disrupting healthy pool members."""
        with self._client_locks[owner]:
            with self._lock:
                if owner >= len(self._clients):
                    raise ValueError("filesystem is closed")
                client = self._clients[owner]
                cwd = self._cwd
                previous_generation = client.generation
            try:
                client.reconnect()
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
        """Reconnect only if the token still belongs to the affected session."""
        for owner in self._tokens_by_owner([token]):
            with self._client_locks[owner]:
                if self.descriptor_valid(token):
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
                self._reconnect_client(owner)
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

    def open(self, path, mode, **kwargs):
        self.ensure_ready()
        self._retry_deferred_closes()
        owner, client = self._choose()
        with self._client_locks[owner]:
            return self._register(owner, client.open(path, mode, **kwargs))

    def open_many(self, paths, modes):
        self.ensure_ready()
        self._retry_deferred_closes()
        owner, client = self._choose()
        with self._client_locks[owner]:
            return [self._register(owner, fd) for fd in client.open_many(paths, modes)]

    def open_many_independent(self, paths, modes):
        """Open descriptors concurrently across independent pooled sessions.

        Unlike open_many, this does not make one vector OPEN: every file
        must remain attached to its own session so later positional operations
        can run concurrently. It is intended for bounded reader pools.
        """
        if len(paths) != len(modes):
            raise ValueError("paths and modes must have equal lengths")
        self.ensure_ready()
        self._retry_deferred_closes()
        if not paths:
            return []
        with self._lock:
            if not self._clients:
                raise ValueError("filesystem is closed")
            pool_size = len(self._clients)
            start = self._next_client
            self._next_client += len(paths)
            owners = [(start + index) % pool_size for index in range(len(paths))]

        opened = [None] * len(paths)
        abandoned = False

        def open_one(index):
            owner = owners[index]
            with self._client_locks[owner]:
                # Shutdown replaces the list before retiring each session.
                clients = self._clients
                if not clients:
                    raise ValueError("filesystem is closed")
                client = clients[owner]
                native_fd = client.open(paths[index], modes[index])
                try:
                    with self._lock:
                        token = self._register(owner, native_fd)
                        if abandoned:
                            # Shutdown may itself have been interrupted. Late
                            # successes still retain explicit cleanup ownership.
                            self._deferred_close.add(token)
                        else:
                            opened[index] = token
                except BaseException:
                    try:
                        client.close(native_fd)
                    except BaseException:
                        pass
                    raise

        failures = []
        futures = []
        try:
            with ThreadPoolExecutor(max_workers=min(len(paths), pool_size)) as executor:
                for index in range(len(paths)):
                    futures.append(executor.submit(open_one, index))
                for future in as_completed(futures):
                    try:
                        future.result()
                    except BaseException as error:
                        failures.append(error)
        except BaseException as error:
            # Even executor shutdown can be interrupted before workers finish.
            failures.insert(0, error)

        if failures:
            # Atomically stop accepting ownership from workers before inspecting
            # the slots. Each success belongs either here or to deferred cleanup.
            with self._lock:
                abandoned = True
            self._close_setup_handles(opened, len(opened))
            raise failures[0]

        return opened

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
                    try:
                        if name == "close" and token in self._deferred_close:
                            result = self._close_deferred_many(client, [native_fd])
                        else:
                            result = getattr(client, name)(native_fd, *args, **kwargs)
                    except BaseException:
                        if name == "close":
                            self.defer_close_many([token])
                        raise
                    if name == "close":
                        self._discard_tokens([token])
                return result

            return fd_call

        if name in self._FD_MANY_METHODS:

            def fd_many_call(tokens, *args, **kwargs):
                self.ensure_ready()
                tokens = tuple(tokens)
                resolved = self._resolve_many(tokens)
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
                        resolved = self._resolve_many(tokens)
                        client = resolved[0][2]
                        native_fds = [native_fd for _, native_fd, _ in resolved]
                    try:
                        if (
                            name == "close_many"
                            and self._native_has_deferred_close
                            and any(token in self._deferred_close for token in tokens)
                        ):
                            kwargs = {**kwargs, "_deferred": True}
                        result = getattr(client, name)(native_fds, *args, **kwargs)
                    except BaseException as error:
                        if name == "close_many":
                            self._discard_confirmed_closes(error, tokens)
                            self.defer_close_many(tokens)
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
                except OSError as error:
                    if (
                        not _can_reconnect(error)
                        or not self._auto_reconnect
                        or name not in _ResilientClient._IDEMPOTENT
                    ):
                        raise
                    self._reconnect_client(owner)
                    return getattr(client, name)(*args, **kwargs)

        return path_call


class _RawVfsiFile(io.RawIOBase):
    """A binary file object backed by an open vnfs descriptor.

    Direct raw construction may defer read OPEN until first I/O. Public scalar
    ``fs.open()`` pins a descriptor at open time, while bulk path operations
    keep their vectorized no-OPEN paths. Write/append modes open eagerly so
    ``wb`` truncates and ``ab`` appends at open time, matching POSIX ``open()``.
    """

    def __init__(self, fs, path, mode="rb", fd=None, identity=None):
        super().__init__()
        # Set these first so close()/__del__ are safe even if later
        # initialization (e.g. an unsupported mode) raises.
        self._closed = False
        self._fd = fd  # None until the first I/O (read modes) or __init__ (w/a)
        self._fd_generation = fs._client.generation if fd is not None else None
        self._identity = identity
        self._identity_required = fd is not None
        self._last_attrs = None
        self._broken = False
        self.fs = fs
        self.path = fs._checked_strip_protocol(path)
        self.mode = mode
        self._base_mode = _normalize_mode(mode)
        self._pos = 0
        self._readable = self._base_mode in ("r", "r+", "w+", "a+", "x+")
        self._writable = self._base_mode in ("r+", "w", "w+", "a", "a+", "x", "x+")
        if fd is None and self._base_mode in ("w", "w+", "a", "a+", "x", "x+"):
            try:
                with self.fs._cache_mutation([self.path], namespace=True):
                    self._ensure_open()
            except BaseException as error:
                _close_reader_on_exit(self, type(error), error, error.__traceback__)
                raise

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

    @property
    def _uses_descriptor(self):
        # A pinned reader still needs its descriptor after a failed reopen.
        return self._fd is not None or self._identity_required

    def _ensure_open(self):
        if self._broken:
            raise ConnectionError("write handle is unusable after a failed operation")
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
            if self._identity_required and self._identity is None:
                raise _stale_handle_error(self.path)
            try:
                fd = self.fs._client.open(
                    self.fs._native_path(self.path), self._native_mode()
                )
            except FileNotFoundError:
                if self._identity_required:
                    raise _stale_handle_error(self.path) from None
                raise
            attrs = None
            identity = None
            try:
                if self._readable or self._base_mode.startswith("a"):
                    attrs = self.fs._client.fstat(fd)
                    if self._readable:
                        identity = _handle_identity(attrs)
                        if self._identity_required and (
                            identity is None or identity != self._identity
                        ):
                            raise _stale_handle_error(self.path)
                    if self._base_mode.startswith("a"):
                        self._pos = _require_size(attrs, self.path)
            except BaseException:
                # Do not publish a reopened descriptor until its identity has
                # been checked. A later call must retry this check, not trust a
                # descriptor whose metadata probe failed.
                try:
                    self.fs._client._close_live_token(fd)
                except BaseException:
                    self.fs._client.defer_close_many([fd])
                raise
            self._fd = fd
            self._fd_generation = self.fs._client.generation
            if attrs is not None:
                self._last_attrs = attrs
                if self._readable and not self._identity_required:
                    self._identity = identity
                    # An open reader is pinned even if the backend cannot
                    # supply enough identity to reopen it after retirement.
                    self._identity_required = True
        return self._fd

    def _pread_at(self, offset, length):
        """Retry an absolute-offset read once on a fresh session."""
        fd = self._ensure_open()
        try:
            return self.fs._client.pread(fd, length, offset)
        except (ConnectionError, OSError) as error:
            stale = not self.fs._client.descriptor_valid(fd)
            if isinstance(error, OSError) and not _is_transport_error(error):
                if error.errno != errno.EBADF or not stale:
                    raise
            if self._writable:
                # Reopening an update handle can repeat create/truncate or
                # switch its identity. Retain the descriptor only for cleanup.
                self._broken = True
                raise
            if not self.fs.auto_reconnect or (
                _is_transport_error(error) and not _can_reconnect(error)
            ):
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
        # Uncached/update handles must observe growth and truncation, including
        # changes through another descriptor. Cached readers keep their own size.
        if not self.closed and self._uses_descriptor:
            fd = self._ensure_open()
            try:
                attrs = self.fs._client.fstat(fd)
            except _RetiredDescriptorError:
                if self._writable or not self.fs.auto_reconnect:
                    raise
                attrs = self.fs._client.fstat(self._ensure_open())
            return _require_size(attrs, self.path)
        return _require_size(self.fs.info(self.path), self.path)

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
        view = _writable_byte_view(b)
        if len(view) == 0:
            return 0
        length = min(len(view), self._MAX_READ)
        self.fs._check_read_allocation(length, self.path)
        data = self._pread(length)
        n = len(data)
        if n:
            view[:n] = data
            self._pos += n
        return n

    def read(self, size=-1):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._readable:
            raise io.UnsupportedOperation("not readable")
        if size is not None:
            size = operator.index(size)
        if size is None or size < 0:
            if not self._uses_descriptor and self._pos == 0:
                # Whole-file read: one batched read, no size stat, no OPEN.
                data, errors = self.fs._client.read_all_many(
                    [self.fs._native_path(self.path)],
                    self.fs.read_all_max_total_bytes,
                )
                if errors:
                    raise _oserror(errors[0], self.path)
                buf = data[0] or b""
                self._pos = len(buf)
                return buf
            # One size fetch + one read loop instead of readall()'s
            # doubling-chunk preads.
            size = max(0, self._size() - self._pos)
            if size == 0:
                return b""
        self.fs._check_read_allocation(size, self.path)
        chunks = []
        remaining = size
        position = self._pos
        while remaining > 0:
            chunk = self._pread_at(position, min(remaining, self._MAX_READ))
            if not chunk:
                break
            chunks.append(chunk)
            position += len(chunk)
            remaining -= len(chunk)
        # An exception from a later chunk leaves the cursor at the start of
        # the call, since no bytes were returned to the application.
        data = b"".join(chunks)
        self._pos = position
        return data

    def write(self, data):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._writable:
            raise io.UnsupportedOperation("not writable")
        if isinstance(data, str):
            raise TypeError("a bytes-like object is required, not 'str'")
        fd = self._ensure_open()
        with self.fs._cache_mutation([self.path], descriptors=[fd]):
            try:
                if self._base_mode.startswith("a"):
                    # O_APPEND: the backend appends regardless of the requested offset.
                    n, new_pos = self.fs._client.write_positioned(fd, data)
                else:
                    n = self.fs._client.pwrite(fd, data, self._pos)
                    new_pos = self._pos + n
            except (OSError, NotImplementedError) as error:
                if not (
                    _is_uncertain_mutation(error) or getattr(error, "bytes_written", 0)
                ):
                    raise
                # A failed write may already have changed the file, even with a
                # definite partial result. The caller can close this handle, but
                # must reconcile the file before attempting more writes.
                self._broken = True
                try:
                    if (
                        self.fs.auto_reconnect
                        and _can_reconnect(error)
                        and self.fs._client.descriptor_valid(fd)
                    ):
                        self.fs._client.reconnect_descriptor(fd)
                except BaseException:
                    # Preserve the ambiguous mutation error; reconnect is cleanup.
                    pass
                finally:
                    # A disabled or failed reconnect can leave the original token
                    # live. Retain it for CLOSE; _broken prevents mutation replay.
                    if not self.fs._client.descriptor_valid(fd):
                        self._fd = None
                        self._fd_generation = None
                raise
            self._pos = new_pos
            return n

    def seek(self, offset, whence=0):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        offset = operator.index(offset)
        whence = operator.index(whence)
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
        else:
            size = operator.index(size)
        if size < 0:
            raise OSError(errno.EINVAL, "Invalid argument")
        fd = self._ensure_open()
        with self.fs._cache_mutation([self.path], descriptors=[fd]):
            try:
                self.fs._client.ftruncate(fd, size)
            except OSError as error:
                if not _is_uncertain_mutation(error):
                    raise
                # The server may have applied the size change before the reply
                # was lost. Never replay it on a potentially different handle.
                self._broken = True
                raise
            return size

    def flush(self):
        # NFS writes are FILE_SYNC (stable); nothing to flush.
        return None

    def close(self):
        if self._closed:
            return
        fd = self._fd
        if fd is not None:
            # A retired session has already released its descriptors. A live
            # session's CLOSE failure retains ownership for retry.
            self.fs._client._close_live_token(fd)
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
        identity=None,
    ):
        # Validate options before a creating/truncating native OPEN.
        self._closed = True
        self._buffer_group = None
        self._base_mode = _normalize_mode(mode)
        self._cache_options = dict(cache_options or {})
        if self._base_mode == "r" and caches.get(cache_type) is KnownPartsOfAFile:
            data = self._cache_options.get("data")
            if isinstance(data, MutableMapping):
                # fsspec consumes this map while consolidating ranges. Each
                # grouped file needs its own copy of the validated data.
                self._cache_options["data"] = dict(data)
        self._raw = _RawVfsiFile(fs, path, mode, fd=fd, identity=identity)
        try:
            if fd is not None and self._base_mode in ("a", "a+") and size is not None:
                self._raw._pos = size
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
            self._write_failure_errno = None
            self._buffer_size = size
            self._requested_read_end = None
            self.cache_type = cache_type
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
                        "none"
                        if self._deferred_cache_type is not None or cache_type == "mmap"
                        else cache_type
                    ),
                    cache_options=(
                        {}
                        if self._deferred_cache_type is not None or cache_type == "mmap"
                        else self._cache_options
                    ),
                    size=size,
                    identity=identity,
                )
                if self._buffered_read and cache_type == "mmap" and not defer_cache:
                    self.cache = _Nfs4MMapCache(
                        self.blocksize,
                        self._fetch_range,
                        self.size,
                        **self._cache_options,
                    )
                if self._buffered_write and self._base_mode == "a":
                    append_size = self._raw.tell() if size is None else size
                    self._raw._pos = append_size
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
        except BaseException as error:
            # The facade may not yet have the fields required by flush/close.
            # Its raw owner is independently closeable throughout setup.
            self._closed = True
            _close_reader_on_exit(self._raw, type(error), error, error.__traceback__)
            raise

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
        if self._has_read_cache() and self._buffer_size is not None:
            return self._buffer_size
        return self._raw.size

    @size.setter
    def size(self, value):
        self._buffer_size = value

    def _initialize_read_cache(self):
        if self._deferred_cache_type is not None:
            cache_type = self._deferred_cache_type
            self._deferred_cache_type = None
            factory = _Nfs4MMapCache if cache_type == "mmap" else caches[cache_type]
            self.cache = factory(
                self.blocksize,
                self._fetch_range,
                self.size,
                **self._cache_options,
            )

    def _attach_group(self, group):
        self._buffer_group = group
        self._initialize_read_cache()
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
        data = self._raw._pread_at(start, max(0, end - start))
        if not data or len(data) >= end - start:
            return data
        # A positive short native read is not EOF. Fixed-block caches must
        # receive the complete range before marking its blocks as populated.
        result = bytearray(data)
        while len(result) < end - start:
            chunk = self._raw._pread_at(start + len(result), end - start - len(result))
            if not chunk:
                break
            result.extend(chunk)
        return bytes(result)

    def _fetch_ranges(self, ranges):
        """Fill sparse cache ranges through this file's pinned descriptor."""
        lengths = [end - start for start, end in ranges]
        self.fs._check_read_allocation(sum(lengths), self.path)
        for batch in _bounded_batches(
            lengths, self.fs.batch_size, self.fs.max_batch_bytes
        ):
            fd = self._raw._ensure_open()
            try:
                data, errors = self.fs._client.pread_many(
                    [fd] * len(batch),
                    [ranges[index][0] for index in batch],
                    [lengths[index] for index in batch],
                )
            except OSError as error:
                if not self.fs.auto_reconnect or (
                    not isinstance(error, _RetiredDescriptorError)
                    and not _can_reconnect(error)
                ):
                    raise
                # Read-only recovery reuses the scalar identity/retry engine.
                for index in batch:
                    yield self._fetch_range(*ranges[index])
                continue
            for position, index in enumerate(batch):
                start, end = ranges[index]
                if position in errors:
                    error = _oserror(errors[position], self.path)
                    if not self.fs.auto_reconnect or not _can_reconnect(error):
                        raise error
                    yield self._fetch_range(start, end)
                    continue
                value = data[position] or b""
                if value and len(value) < lengths[index]:
                    value += self._fetch_range(start + len(value), end)
                yield value

    def _initiate_upload(self):
        # Creation/truncation and append positioning happen when the raw
        # descriptor is opened, preserving POSIX open-time errors.
        return None

    def _raise_write_failure(self):
        message = "buffered writer is unusable after a failed flush"
        if self._write_failure_errno is not None:
            raise OSError(self._write_failure_errno, message, self.path)
        raise ConnectionError(message)

    def _reconcile_append_position(self, previous, written, position):
        # loc also counts bytes still buffered/spooled. Adjust only the gap
        # between the predicted append end and the backend's actual end.
        self.loc += position - (previous + written)
        self._raw._pos = position

    def _mark_write_failure(self, error):
        self._write_failed = True
        if isinstance(error, OSError) and not _is_transport_error(error):
            self._write_failure_errno = error.errno
        self._raw._broken = True

    def _fail_staging(self, error):
        self._mark_write_failure(error)
        # These bytes are unusable after a partial local write. Release them,
        # retaining any spool whose close fails for group cleanup to retry.
        self.buffer = io.BytesIO()
        if self._write_spool is not None:
            try:
                self._write_spool.close()
            except BaseException as cleanup:
                raise error from cleanup
            self._write_spool = None

    def _upload_chunk(self, final=False):
        data = self.buffer.getvalue()
        offset = 0
        try:
            while offset < len(data):
                previous = self._raw.tell()
                written = self._raw.write(data[offset:])
                if written <= 0:
                    raise OSError(errno.EIO, "short buffered write", self.path)
                if self._base_mode == "a":
                    self._reconcile_append_position(previous, written, self._raw.tell())
                offset += written
        except BaseException as error:
            self._mark_write_failure(error)
            raise
        return True

    def _stage_buffer(self):
        if self.buffer.tell() == 0:
            return
        if self._write_spool is None:
            raise RuntimeError("grouped writer has no staging spool")
        try:
            self._write_spool.seek(0, io.SEEK_END)
            _write_fully(self._write_spool, self.buffer.getvalue())
            self.buffer = io.BytesIO()
            if self._buffer_group is not None:
                self._buffer_group.enforce_memory_limit()
        except BaseException as error:
            self._fail_staging(error)
            raise

    def read(self, size=-1):
        if not self._has_read_cache():
            return self._raw.read(size)
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if size is not None:
            size = operator.index(size)
        if (
            self._buffered_read
            and self.cache_type != "blockcache"
            and (size is None or size < 0)
            and self.loc == 0
            and self._cache_is_pristine()
        ):
            if self._buffer_group is not None and self.fs.vectorized_buffering:
                data = self._buffer_group.read_all(self)
            elif self._raw._uses_descriptor:
                # A scalar open pins a native descriptor. A path-based
                # read_all_many could switch to a replacement inode after a
                # rename, even on a handle that was already read once.
                self._raw.seek(0)
                data = self._raw.read(self.size)
            else:
                data, errors = self.fs._client.read_all_many(
                    [self.fs._native_path(self.path)],
                    self.fs.read_all_max_total_bytes,
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

    def readuntil(self, char=b"\n", blocks=None):
        """Read through a delimiter without exceeding the returned-data budget."""
        return self._read_until(char, blocks, -1)

    def _read_until(self, char, blocks, size):
        if size == 0:
            return self.read(0)
        if not isinstance(char, bytes):
            char = bytes(memoryview(char))
        if not char:
            raise ValueError("delimiter must not be empty")
        block_size = self.blocksize if blocks is None else operator.index(blocks)
        if block_size <= 0:
            raise ValueError("blocks must be positive")
        limit = self.fs.read_all_max_total_bytes
        if limit == 0:
            self.read(0)  # Validate closed/readable state without consuming data.
            # Cached readers retain their per-open EOF. Uncached readers need
            # a one-byte positional probe, not an unconditional read(0) EOF.
            remaining = (
                self.tell() < self.size
                if self._has_read_cache()
                else bool(self._raw._pread_at(self.tell(), 1))
            )
            self.fs._check_read_allocation(int(remaining), self.path)
            return b""
        data = bytearray()
        start = self.tell()
        try:
            while True:
                previous = len(data)
                requested = min(block_size, limit - previous + 1, limit)
                if size >= 0:
                    requested = min(requested, size - previous)
                part = self.read(requested)
                if not part:
                    return bytes(data)
                data.extend(part)
                found = data.find(char, max(0, previous - len(char) + 1))
                if found >= 0:
                    end = found + len(char)
                    self.fs._check_read_allocation(end, self.path)
                    self.seek(start + end)
                    return bytes(data[:end])
                self.fs._check_read_allocation(len(data), self.path)
                if size >= 0 and len(data) == size:
                    return bytes(data)
        except OSError as error:
            if error.errno == errno.EFBIG:
                self.seek(start)
            raise

    def readline(self, size=-1):
        size = -1 if size is None else operator.index(size)
        return self._read_until(b"\n", None, size)

    def readinto(self, b):
        if not self._has_read_cache():
            return self._raw.readinto(b)
        if self.closed:
            raise ValueError("I/O operation on closed file")
        return super().readinto(_writable_byte_view(b))

    def write(self, data):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._using_buffer:
            return self._raw.write(data)
        if not self._buffered_write:
            return super().write(data)
        if isinstance(data, str):
            raise TypeError("a bytes-like object is required, not 'str'")
        if self._write_failed:
            self._raise_write_failure()
        if self.forced:
            raise ValueError("This file has been force-flushed, can only close")
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
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._has_read_cache() and not self._buffered_write:
            return self._raw.seek(offset, whence)
        requested = operator.index(offset)
        whence = operator.index(whence)
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
        return super().seek(requested, whence)

    def tell(self):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._has_read_cache() and not self._buffered_write:
            return self._raw.tell()
        return super().tell()

    def truncate(self, size=None):
        if not self._using_buffer:
            return self._raw.truncate(size)
        if self._buffered_write:
            if self.forced:
                raise ValueError("This file has been force-flushed, can only close")
            self.flush()
            if size is None:
                size = self.tell()
            return self._raw.truncate(size)
        raise io.UnsupportedOperation("not writable")

    def flush(self, force=False):
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not self._using_buffer:
            return self._raw.flush()
        if self._buffered_read:
            return None
        if self._write_failed:
            self._raise_write_failure()
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
        if not self._buffer_group._finalizing_wrappers:
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
        group = self._buffer_group
        if group is not None and group._finalizing_wrappers and self._raw.writable():
            # Encoders can close their sink while still producing final bytes.
            # Keep native handles live until the group has drained all spools.
            if self._buffered_write and not self.closed and not self._write_failed:
                self._stage_buffer()
            return
        if self.closed:
            group = self._buffer_group
            if group is not None and not group._group_closed:
                group.request_close(self)
            return
        if self._write_failed and self._buffer_group is None:
            # A failed upload may have committed a prefix. Release any remaining
            # descriptor without flushing or replaying its buffered bytes.
            # If CLOSE fails, retain ownership so cleanup is still retryable.
            self._raw.close()
            self._closed = True
            self._raise_write_failure()
        group = self._buffer_group
        if group is not None:
            if self._buffered_write:
                try:
                    self.flush(force=True)
                except BaseException as error:
                    # A failed writer must still join cohort cleanup, without
                    # replaying its data or closing healthy siblings early.
                    try:
                        group.request_close(self)
                    except BaseException as cleanup:
                        raise error from cleanup
                    self._closed = True
                    raise
            elif self._has_read_cache():
                cache = getattr(self, "cache", None)
                close = getattr(cache, "close", None)
                if callable(close):
                    close()
                self.cache = None
            group.request_close(self)
            self._closed = True
            return
        if self._buffered_write and not self.forced:
            try:
                self.flush(force=True)
            except BaseException as error:
                # A final upload may commit a prefix before failing. Close the
                # native handle without replaying data, and preserve the write
                # error if cleanup also fails.
                _close_reader_on_exit(
                    self._raw, type(error), error, error.__traceback__
                )
                self._closed = True
                raise
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
        # Retain references if a local close fails so explicit cleanup can retry.
        # ExitStack still attempts both resources and preserves the first error.
        with ExitStack() as stack:
            for resource in (self._write_spool, getattr(self, "cache", None)):
                if callable(getattr(resource, "close", None)):
                    stack.push(partial(_close_reader_on_exit, resource))
        self._write_spool = None
        self.cache = None


class _WrappedFiles(list):
    """Grouped binary handles and their fsspec text/compression wrappers."""

    def __init__(self, raw_files, open_files):
        super().__init__()
        self.raw_files = raw_files
        self.open_files = open_files
        self.group = raw_files[0]._buffer_group
        self.group._wrapped_files = self

    def wrap(self):
        for file, open_file in zip(self.raw_files, self.open_files):
            if "r" not in open_file.mode:
                # OpenFiles explicitly invokes each entry's __exit__ before
                # commit_many. Hand that phase to the existing coordinator;
                # leave close() and the standalone context-manager protocol alone.
                open_file._vfsi_previous_exit = open_file.__dict__.get("__exit__")
                open_file.__exit__ = self.group._exit_wrapped
            open_file.fobjects = [file]
            if open_file.compression is not None:
                file = _OwnedStream(
                    compr[open_file.compression](file, mode=open_file.mode[0]), file
                )
                open_file.fobjects.append(file)
            if "b" not in open_file.mode:
                file = _TextIOWrapper(
                    file,
                    encoding=open_file.encoding,
                    errors=open_file.errors,
                    newline=open_file.newline,
                )
                open_file.fobjects.append(file)
            # A slice or concatenation of the returned list can still commit
            # this group through any member, including a wrapped stream.
            file._buffer_group = self.group
            self.append(file)
        return self


class _BufferGroup:
    """Coordinate cache misses and buffered flushes across OpenFiles."""

    def __init__(self, fs, files):
        self.fs = fs
        self.files = list(files)
        self._lock = threading.RLock()
        self._ranges = {}
        self._whole_files = {}
        self._speculative_order = OrderedDict()
        self._speculative_bytes = 0
        self._close_requested = set()
        self._group_closed = False
        self._wrapped_files = None
        self._finalizing_wrappers = False
        self._finalized = False
        self._finalization_error = None
        for file in self.files:
            file._attach_group(self)

    def _take_speculative(self, whole, key):
        store = self._whole_files if whole else self._ranges
        value = store.pop(key, None)
        if value is not None:
            self._speculative_bytes -= len(value)
            self._speculative_order.pop((whole, key), None)
        return value

    def _store_speculative(self, whole, key, value):
        # Bound retained prefetch across all waves, not just each vector RPC.
        budget = min(self.fs.max_batch_bytes, self.fs.read_all_max_total_bytes)
        self._take_speculative(whole, key)
        if not value or len(value) > budget:
            return
        while self._speculative_bytes + len(value) > budget:
            oldest = next(iter(self._speculative_order))
            self._take_speculative(*oldest)
        store = self._whole_files if whole else self._ranges
        store[key] = value
        self._speculative_order[(whole, key)] = None
        self._speculative_bytes += len(value)

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
                if spool is not None and not spool.closed and not spool._rolled:
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
                if spool is not None and not spool.closed and not spool._rolled:
                    position = spool.tell()
                    size = spool.seek(0, io.SEEK_END)
                    spool.seek(position)
                    if size:
                        try:
                            spool.rollover()
                        except BaseException as error:
                            file._fail_staging(error)
                            raise

    def _file_index(self, file):
        return next(i for i, member in enumerate(self.files) if member is file)

    def _rebase_file_error(self, error, files):
        position = getattr(error, "index", None)
        if (
            isinstance(position, int)
            and not isinstance(position, bool)
            and 0 <= position < len(files)
        ):
            file = files[position]
            _oserror(error, file.path, index=self._file_index(file))

    @staticmethod
    def _split_read_outcomes(outcomes):
        data = []
        errors = {}
        for index, result in enumerate(outcomes):
            if isinstance(result, BaseException):
                errors[index] = result
                data.append(None)
            else:
                data.append(result)
        return data, errors

    def _read_independently(
        self, method, files, offsets, lengths, setup_errors, *, retry_setup=False
    ):
        # Keep speculative failures local to their own input. For modern
        # adapters, a single-item vector preserves native read progress and
        # leaves suffix recovery to the same coordinator as a batched read.
        client = self.fs._client
        outcomes = []
        for file, offset, length in zip(files, offsets, lengths):
            setup_error = setup_errors.get(id(file)) if setup_errors else None
            if setup_error is not None and not (
                retry_setup and _can_reconnect(setup_error)
            ):
                # This file never dispatched READ. Keep its setup diagnostic
                # distinct from any other reader restored in the same call.
                error = copy(setup_error)
                error.__cause__ = setup_error
                error.filename = file.path
                error.index = self._file_index(file)
                error.failure_state = "not_attempted"
                outcomes.append(error)
                continue
            try:
                if method == "preadv":
                    fd = file._raw._ensure_open()
                    try:
                        values = client.preadv([fd], [offset], [length])
                    except _RetiredDescriptorError:
                        if file._raw.writable() or not self.fs.auto_reconnect:
                            raise
                        fd = file._raw._ensure_open()
                        values = client.preadv([fd], [offset], [length])
                else:
                    outcomes.append(file._raw._pread_at(offset, length))
                    continue
            except (OSError, NotImplementedError) as error:
                if method == "preadv":
                    # Setup or outer vector rejection did not execute READ.
                    error.failure_state = "not_attempted"
                outcomes.append(error)
                continue
            if len(values) != 1:
                raise OSError(errno.EIO, "invalid positioned-read outcome count")
            outcomes.append(values[0])
        return outcomes

    def _read_vector(self, method, files, offsets, lengths, *, reconnect=False):
        client = self.fs._client
        # Fork recovery can invalidate the whole pool. It must succeed before
        # any descriptor can be treated as healthy.
        client.ensure_ready()
        setup_errors = None
        try:
            if reconnect or (
                self.fs.auto_reconnect
                and any(not client.descriptor_valid(file._raw._fd) for file in files)
            ):
                setup_errors = self._reopen_readers(files, reconnect=reconnect)
            if setup_errors is None:
                fds = [file._raw._ensure_open() for file in files]
                one_session = (
                    self.fs.connection_pool_size == 1
                    or len({client._resolve(fd)[0] for fd in fds}) <= 1
                )
            else:
                one_session = False
        except OSError as error:
            if _is_transport_error(error):
                raise
        else:
            if one_session:
                try:
                    return getattr(client, method)(fds, offsets, lengths)
                except Exception as error:
                    self._rebase_file_error(error, files)
                    raise
        # Setup can fail for a single file, or previous scalar recovery may
        # have distributed pinned handles among native sessions.
        outcomes = self._read_independently(
            method,
            files,
            offsets,
            lengths,
            setup_errors,
            retry_setup=method == "pread_many"
            and self.fs.auto_reconnect
            and not reconnect,
        )
        if method == "preadv":
            return outcomes
        return self._split_read_outcomes(outcomes)

    def _reopen_readers(self, files, *, reconnect=False):
        client = self.fs._client
        active = [
            file
            for file in files
            if not file.closed and file.readable() and not file._raw.writable()
        ]
        errors = {}
        if reconnect:
            by_token = {}
            for file in active:
                by_token.setdefault(file._raw._fd, []).append(file)
            for tokens in client._tokens_by_owner(by_token).values():
                try:
                    client.reconnect_descriptor(tokens[0])
                except OSError as error:
                    errors.update(
                        (id(file), error)
                        for token in tokens
                        for file in by_token[token]
                    )
        # Failed owners cannot supply this retry even if their old token is
        # still registered. Leave other live tokens on their original owners.
        active = [
            file
            for file in active
            if id(file) not in errors and not client.descriptor_valid(file._raw._fd)
        ]
        if not active:
            return errors or None
        fds = []
        try:
            fds = client.open_many(
                [self.fs._native_path(file.path) for file in active],
                ["rb"] * len(active),
            )
            if len(fds) != len(active):
                raise OSError(errno.EIO, "invalid reopened descriptor count")
            attrs = client.fstat_many(fds)
            if len(attrs) != len(active):
                raise OSError(errno.EIO, "invalid reopened metadata count")
            for file, attr in zip(active, attrs):
                identity = _handle_identity(attr)
                if file._raw._identity is None or identity != file._raw._identity:
                    raise _stale_handle_error(file.path)
        except BaseException as error:
            index = getattr(error, "index", None)
            self._rebase_file_error(error, active)
            if fds:
                try:
                    client.close_many(fds)
                except BaseException:
                    client.defer_close_many(fds)
            if not isinstance(error, (OSError, NotImplementedError)):
                raise
            if isinstance(error, NotImplementedError) or _is_transport_error(error):
                # An attributed setup failure leaves other stale readers
                # eligible for independent identity-checked reopen.
                affected = (
                    [active[index]]
                    if type(index) is int and 0 <= index < len(active)
                    else active
                )
                errors.update((id(file), error) for file in affected)
            # A dict (including empty) requests independent fallback. None
            # below denotes a successful restore with no per-file failures.
            return errors
        for file, fd in zip(active, fds):
            file._raw._fd = fd
            file._raw._fd_generation = client.generation
        return errors or None

    @staticmethod
    def _prepend_read_prefix(error, prefix, offset):
        error.read_prefix = bytes(prefix) + getattr(error, "read_prefix", b"")
        error.read_offset = offset
        if not hasattr(error, "read_eof"):
            error.read_eof = False
        if getattr(error, "failure_state", None) == "not_attempted":
            error.failure_state = "failed"

    def _read_error_matches_file(self, error, file):
        index = getattr(error, "index", None)
        if index is not None and (
            type(index) is not int or index != self._file_index(file)
        ):
            return False
        filename = error.filename
        if filename is None:
            return True
        if filename not in (file.path, self.fs._native_path(file.path)):
            return False
        return index is not None or not any(
            member is not file
            and filename in (member.path, self.fs._native_path(member.path))
            for member in self.files
        )

    def _pread_many(self, files, offsets, lengths, *, required=None):
        data, errors = self._pread_many_once(files, offsets, lengths, required=required)
        # Speculation gets one wave. Complete only required cache fills;
        # internal callers without a selected file require every result.
        partial = {
            index: bytearray(value)
            for index, value in enumerate(data)
            if index not in errors
            and value
            and len(value) < lengths[index]
            and (required is None or files[index] is required)
        }
        pending = list(partial)
        while pending:
            try:
                more, failures = self._pread_many_once(
                    [files[index] for index in pending],
                    [offsets[index] + len(partial[index]) for index in pending],
                    [lengths[index] - len(partial[index]) for index in pending],
                )
            except OSError as error:
                # A grouped reopen can fail on a different file. Only attach
                # progress when the exception can describe this continuation.
                if len(pending) == 1:
                    index = pending[0]
                    if self._read_error_matches_file(error, files[index]):
                        self._prepend_read_prefix(error, partial[index], offsets[index])
                raise
            remaining = []
            for position, index in enumerate(pending):
                if position in failures:
                    error = _oserror(failures[position], files[index].path)
                    self._prepend_read_prefix(error, partial[index], offsets[index])
                    errors[index] = error
                    data[index] = None
                elif more[position]:
                    partial[index].extend(more[position])
                    if len(partial[index]) < lengths[index]:
                        remaining.append(index)
            pending = remaining
        for index, value in partial.items():
            if index not in errors:
                data[index] = bytes(value)
        return data, errors

    def _read_vector_with_recovery(self, method, files, offsets, lengths):
        try:
            return self._read_vector(method, files, offsets, lengths)
        except OSError as error:
            # These outer errors reject before dispatch. If a token already
            # retired, reopen readers without replacing the healthy session.
            retired = isinstance(error, _RetiredDescriptorError)
            if not self.fs.auto_reconnect or (
                not retired and not _can_reconnect(error)
            ):
                raise
            return self._read_vector(
                method, files, offsets, lengths, reconnect=not retired
            )

    def _pread_many_once(self, files, offsets, lengths, *, required=None):
        if getattr(self.fs._client, "_native_has_vector_outcomes", False):
            outcomes = self._read_vector_with_recovery(
                "preadv", files, offsets, lengths
            )
            if len(outcomes) != len(files):
                raise OSError(errno.EIO, "invalid positioned-read outcome count")
            retry = [
                index
                for index, result in enumerate(outcomes)
                if isinstance(result, OSError)
                and (required is None or files[index] is required)
                and self.fs.auto_reconnect
                and _can_reconnect(result)
                and not files[index]._raw.writable()
                and not getattr(result, "read_eof", False)
                and len(getattr(result, "read_prefix", b"")) < lengths[index]
            ]
            if retry:
                prefixes = {
                    index: getattr(outcomes[index], "read_prefix", b"")
                    for index in retry
                }
                try:
                    resumed = self._read_vector(
                        "preadv",
                        [files[index] for index in retry],
                        [offsets[index] + len(prefixes[index]) for index in retry],
                        [lengths[index] - len(prefixes[index]) for index in retry],
                        reconnect=True,
                    )
                except OSError:
                    # Reconnect cannot erase successful siblings or replace the
                    # original failure when recovery itself is unavailable.
                    resumed = [outcomes[index] for index in retry]
                    prefixes = {index: b"" for index in retry}
                if len(resumed) != len(retry):
                    raise OSError(errno.EIO, "invalid resumed-read outcome count")
                for index, result in zip(retry, resumed):
                    prefix = prefixes[index]
                    if isinstance(result, BaseException):
                        if getattr(result, "failure_state", None) == "not_attempted":
                            # A rejected suffix did not replace the first read failure.
                            continue
                        if prefix:
                            self._prepend_read_prefix(result, prefix, offsets[index])
                    else:
                        result = prefix + result
                    outcomes[index] = result
            return self._split_read_outcomes(outcomes)
        return self._read_vector_with_recovery("pread_many", files, offsets, lengths)

    def fetch(self, current, start, end):
        if end <= start:
            return b""  # mmap may request an empty block exactly at EOF.
        with self._lock:
            key = (id(current), start, end)
            prefetched = self._take_speculative(False, key)
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
                    self._take_speculative(False, cached_key)
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

            data, errors = self._pread_many(files, offsets, lengths, required=current)
            current_data = None
            for index, (file, length, value) in enumerate(zip(files, lengths, data)):
                if index in errors:
                    if file is current:
                        raise _oserror(
                            errors[index],
                            current.path,
                            index=self._file_index(current),
                        )
                    continue
                value = value or b""
                if file is current:
                    current_data = value
                elif len(value) == length:
                    # Never mark a short speculative prefix as a full block.
                    self._store_speculative(
                        False, (id(file), start, start + length), value
                    )
            if current_data is None:
                raise OSError(errno.EIO, "buffered read returned no data", current.path)
            return current_data

    def read_all(self, current):
        with self._lock:
            self.fs._check_read_allocation(current.size, current.path)
            # Buffered readers use their open-time size snapshot. Empty files
            # need neither a READ nor a speculative entry to remember EOF.
            if current.size == 0:
                return b""
            prefetched = self._take_speculative(True, id(current))
            if prefetched is not None:
                return prefetched
            # A whole-file read supersedes a speculative prefix left for this
            # handle. Drop it instead of retaining stale group memory.
            identity = id(current)
            for key in [key for key in self._ranges if key[0] == identity]:
                self._take_speculative(False, key)
            files = []
            lengths = []
            total = 0
            for file in self._active_readers(current):
                if file is not current and self._has_speculative_data(file):
                    continue
                length = (
                    file.size if file is current else min(file.size, file.blocksize)
                )
                if length == 0:
                    continue
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
            data, errors = self._pread_many(
                files, [0] * len(files), lengths, required=current
            )
            current_data = None
            for index, (file, length, value) in enumerate(zip(files, lengths, data)):
                if index in errors:
                    if file is current:
                        raise _oserror(
                            errors[index],
                            current.path,
                            index=self._file_index(current),
                        )
                    continue
                value = value or b""
                if file is current:
                    current_data = value
                elif len(value) == length == file.size:
                    self._store_speculative(True, id(file), value)
                elif len(value) == length:
                    self._store_speculative(False, (id(file), 0, len(value)), value)
            if current_data is None:
                raise OSError(errno.EIO, "buffered read returned no data", current.path)
            return current_data

    def flush_files(self, files):
        pending = []
        first_error = None
        for file in files:
            if file._write_failed or file._write_spool is None:
                continue
            try:
                end = file._write_spool.seek(0, io.SEEK_END)
            except OSError as error:
                file._mark_write_failure(error)
                first_error = first_error or _oserror(
                    error, file.path, index=self.files.index(file)
                )
                continue
            if file._spool_read_offset < end:
                pending.append((file, end))
        while pending:
            wave = []
            datas = []
            total = 0
            for file, end in pending:
                # Full waves cannot admit later files. Avoid reading payloads
                # only to discard and reread them in every subsequent wave.
                if len(wave) >= self.fs.batch_size or total == self.fs.max_batch_bytes:
                    break
                if wave and (file._base_mode == "a") != (wave[0]._base_mode == "a"):
                    continue
                length = min(
                    file.blocksize,
                    self.fs.max_batch_bytes,
                    end - file._spool_read_offset,
                )
                if total + length > self.fs.max_batch_bytes:
                    continue
                try:
                    file._write_spool.seek(file._spool_read_offset)
                    data = file._write_spool.read(length)
                except OSError as error:
                    file._mark_write_failure(error)
                    first_error = first_error or _oserror(
                        error, file.path, index=self.files.index(file)
                    )
                    continue
                if not data:
                    error = OSError(
                        errno.EIO, "incomplete buffered write spool", file.path
                    )
                    file._mark_write_failure(error)
                    first_error = first_error or _oserror(
                        error, file.path, index=self.files.index(file)
                    )
                    continue
                wave.append(file)
                datas.append(data)
                total += len(data)
            if not wave:
                break
            try:
                fds = [file._raw._ensure_open() for file in wave]
                with self.fs._cache_mutation(
                    [file.path for file in wave], descriptors=fds
                ):
                    if getattr(self.fs._client, "_native_has_vector_outcomes", False):
                        appending = wave[0]._base_mode == "a"
                        outcomes = (
                            self.fs._client.appendv(fds, datas)
                            if appending
                            else self.fs._client.pwritev(
                                fds, [file._raw._pos for file in wave], datas
                            )
                        )
                        if len(outcomes) != len(wave):
                            raise OSError(
                                errno.EIO, "invalid buffered-write outcome count"
                            )
                        for file, data, result in zip(wave, datas, outcomes):
                            if isinstance(result, BaseException):
                                # A failed append prefix has no trustworthy actual
                                # position. Retain its error and never replay it.
                                file._mark_write_failure(result)
                                first_error = first_error or _oserror(
                                    result, file.path, index=self.files.index(file)
                                )
                                continue
                            written = result[0] if appending else result
                            if appending:
                                file._reconcile_append_position(
                                    file._raw._pos, written, result[1]
                                )
                            else:
                                file._raw._pos += written
                            file._spool_read_offset += written
                            if written != len(data):
                                error = OSError(
                                    errno.EIO, "short buffered write", file.path
                                )
                                error.failure_state = "failed"
                                error.bytes_written = written
                                file._mark_write_failure(error)
                                first_error = first_error or error
                    elif wave[0]._base_mode == "a":
                        results = self.fs._client.append_many(fds, datas)
                        for file, data, (written, position) in zip(
                            wave, datas, results
                        ):
                            if written != len(data):
                                raise OSError(
                                    errno.EIO, "short buffered append", file.path
                                )
                            file._reconcile_append_position(
                                file._raw._pos, written, position
                            )
                            file._spool_read_offset += written
                    else:
                        offsets = [file._raw._pos for file in wave]
                        results = self.fs._client.pwrite_many(fds, offsets, datas)
                        for file, data, written in zip(wave, datas, results):
                            if written != len(data):
                                raise OSError(
                                    errno.EIO, "short buffered write", file.path
                                )
                            file._raw._pos += written
                            file._spool_read_offset += written
            except BaseException as error:
                index = getattr(error, "index", None)
                if (
                    isinstance(index, int)
                    and not isinstance(index, bool)
                    and 0 <= index < len(wave)
                ):
                    try:
                        error.filename = wave[index].path
                        error.index = self.files.index(wave[index])
                    except (AttributeError, TypeError):
                        pass
                for file in wave:
                    file._write_failed = True
                    file._raw._broken = True
                raise
            pending = [
                (file, end)
                for file, end in pending
                if not file._write_failed and file._spool_read_offset < end
            ]
        if first_error is not None:
            raise first_error

    def _exit_wrapped(self, *exc):
        self.finish()

    def finish(self):
        """Finalize encoders once, drain bounded write waves, then close all."""
        with self._lock:
            if not self._finalized:
                try:
                    if self._wrapped_files is not None:
                        self._finalize_wrappers()
                    buffered = []
                    for file in self.files:
                        if not file._buffered_write or file.closed:
                            continue
                        try:
                            if file._write_failed:
                                file._raise_write_failure()
                            file._stage_buffer()
                        except Exception as error:
                            self._finalization_error = self._finalization_error or error
                        else:
                            buffered.append(file)
                    self.flush_files(buffered)
                except BaseException as error:
                    self._finalization_error = self._finalization_error or error
                finally:
                    self._finalized = True
                    if self._wrapped_files is not None:
                        # Encoders have finished even when native CLOSE needs a
                        # retry. Reject new writes; retain descriptors for cleanup.
                        for file in self.files:
                            if file._raw.writable():
                                file._closed = True
            try:
                self.close_all()
            except BaseException as cleanup_error:
                if self._finalization_error is not None:
                    raise self._finalization_error from cleanup_error
                raise
            if self._finalization_error is not None:
                raise self._finalization_error

    def _finalize_wrappers(self):
        self._finalizing_wrappers = True
        try:
            for file, entry in zip(self.files, self._wrapped_files.open_files):
                try:
                    # Continue down the chain after an encoder fails, and then
                    # continue with every sibling. Never retry failed encoders.
                    for stream in reversed(entry.fobjects):
                        try:
                            if not stream.closed:
                                stream.close()
                        except BaseException as error:
                            self._finalization_error = self._finalization_error or error
                            file._write_failed = True
                            file._raw._broken = True
                finally:
                    entry.fobjects.clear()
                    if hasattr(entry, "_vfsi_previous_exit"):
                        previous = entry._vfsi_previous_exit
                        if previous is None:
                            del entry.__exit__
                        else:
                            entry.__exit__ = previous
                        del entry._vfsi_previous_exit
        finally:
            self._finalizing_wrappers = False

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
            members = {
                member._raw._fd: member
                for member in self.files
                if member._raw._fd is not None
            }
            client = self.fs._client
            by_owner = client._tokens_by_owner(members)
            first_error = None
            if not client.closed:
                for owner, tokens in by_owner.items():
                    for start in range(0, len(tokens), self.fs.batch_size):
                        fds = tokens[start : start + self.fs.batch_size]
                        batch = [members[fd] for fd in fds]
                        try:
                            client._close_live_tokens(owner, fds)
                        except BaseException as error:
                            _finish_group_files(
                                [
                                    member
                                    for member in batch
                                    if not self.fs._client.descriptor_valid(
                                        member._raw._fd
                                    )
                                ]
                            )
                            first_error = first_error or error
                        else:
                            local_error = _finish_group_files(batch)
                            first_error = first_error or local_error
            if first_error is not None:
                raise first_error

            local_error = _finish_group_files(self.files)
            if local_error is not None:
                raise local_error
            self._group_closed = True
            self._ranges.clear()
            self._whole_files.clear()
            self._speculative_order.clear()
            self._speculative_bytes = 0


class _DeferredWriteFile:
    """Disk-spooled transactional write staged beside its destination."""

    def __init__(self, fs, path, mode="wb", *, create_parents=False):
        if not fs._native_has_create_permissions:
            raise ImportError(
                "transactional writes require native private-create support; "
                "upgrade nfs4fs or vsmb alongside vfsi-fsspec"
            )
        permission_capability = getattr(fs._native_module, "CAP_POSIX_METADATA", 0)
        if not permission_capability or not (
            fs._client.capabilities() & permission_capability
        ):
            raise NotImplementedError(
                "transactional writes require backend private-create and "
                "POSIX permission support"
            )
        self.fs = fs
        self.path = path  # internal path
        self.mode = mode
        self._create_parents = create_parents
        self._spool = tempfile.SpooledTemporaryFile(
            max_size=fs.transaction_spool_threshold, mode="w+b"
        )
        with ExitStack() as stack:
            # Construction can fail before transaction enrollment. Own the
            # local spool until append seeding and initialization both succeed.
            stack.push(partial(_close_reader_on_exit, self._spool))
            self._closed = False
            self._committed = False
            self._prepared = False
            self._stage_owned = False
            self._staging_error = None
            self._published = False
            parent = posixpath.dirname(path) or "/"
            # Keep the staging component independent of the destination length;
            # a valid NAME_MAX-sized basename must still be writable atomically.
            self.temp_path = posixpath.join(parent, f".nfs4fs-txn-{uuid.uuid4().hex}")
            if "a" in mode and self._run_scalar(fs.exists, path):
                fs._copy_remote_to_fileobj(path, self._spool)
                self._spool.seek(0, io.SEEK_END)
            stack.pop_all()

    def _run_scalar(self, operation, *args):
        try:
            return operation(*args)
        except Exception as error:
            # A scalar handle must not expose its private operation's vector index.
            position = getattr(error, "index", None)
            if (
                isinstance(position, int)
                and not isinstance(position, bool)
                and position == 0
            ):
                del error.index
            raise

    def _update_spool(self, operation, *args, allow_invalid=False):
        if self._staging_error is not None:
            raise self._staging_error
        try:
            return operation(*args)
        except (OSError, KeyboardInterrupt, SystemExit) as error:
            # Rolled spools report invalid seeks/truncations as EINVAL.
            if not (
                allow_invalid
                and isinstance(error, OSError)
                and error.errno == errno.EINVAL
            ):
                self._staging_error = error
            raise

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
        if "a" in self.mode:
            self._update_spool(self._spool.seek, 0, io.SEEK_END)
        return self._update_spool(self._spool.write, data)

    def seek(self, offset, whence=0):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        return self._update_spool(self._spool.seek, offset, whence, allow_invalid=True)

    def tell(self):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        return self._spool.tell()

    def truncate(self, size=None):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        return self._update_spool(self._spool.truncate, size, allow_invalid=True)

    def flush(self):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        self._update_spool(self._spool.flush)

    def __enter__(self):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        return self

    def __exit__(self, *args):
        self.close()

    def close(self):
        # The spool is retained until the transaction prepares or discards it.
        self._closed = True

    @staticmethod
    def discard_many(files):
        error = None
        for file in files:
            try:
                file.discard()
            except BaseException as exc:
                error = error or exc
        if error is not None:
            raise error

    def prepare(self):
        """Upload to a same-directory temporary file without exposing target."""
        if self._staging_error is not None:
            raise self._staging_error
        if self._prepared or self._committed:
            return
        if "x" in self.mode and self._run_scalar(self.fs.exists, self.path):
            raise FileExistsError(errno.EEXIST, "File exists", self.path)
        try:
            permissions = (
                self.fs._client.stat(self.fs._native_path(self.path))["mode"] & 0o777
            )
        except FileNotFoundError:
            permissions = None  # New transactional files stay private.
        if self._create_parents or self.fs.auto_mkdir:
            self._run_scalar(
                self.fs._ensure_dirs, [posixpath.dirname(self.temp_path) or "/"]
            )
        self._spool.seek(0)
        # Cleanup is permitted only after exclusive OPEN acknowledges creation.
        # A collision or lost OPEN reply does not establish ownership of the name.
        fd = self.fs._client.open(
            self.fs._native_path(self.temp_path), "xb", permissions=0o600
        )
        self._stage_owned = True
        try:
            with ExitStack() as stack:
                remote = VfsiFile(self.fs, self.temp_path, "xb", fd=fd)
                stack.push(partial(_close_reader_on_exit, remote))
                while True:
                    chunk = self._spool.read(self.fs.transfer_chunk_size)
                    if not chunk:
                        break
                    _write_fully(remote, chunk)
            if permissions is not None:
                self.fs._client.chmod(self.fs._native_path(self.temp_path), permissions)
        except BaseException as primary:
            try:
                self._run_scalar(self.fs.rm, self.temp_path)
            except FileNotFoundError:
                self._stage_owned = False
            except OSError:
                # Retain ownership so transaction/manual discard can retry.
                pass
            except BaseException as cleanup:
                raise primary from cleanup
            else:
                self._stage_owned = False
            raise
        self._prepared = True

    def finalize(self):
        self._run_scalar(self.finalize_many, [self])

    def _label_publication_error(self, error, index):
        _oserror(error, self.path, index=index)
        if isinstance(error, OSError):
            error.filename = self.path

    @staticmethod
    def finalize_many(files):
        pending = [
            (index, file) for index, file in enumerate(files) if not file._committed
        ]
        if not pending:
            return
        fs = pending[0][1].fs
        with fs._cache_mutation((file.path for _, file in pending), namespace=True):
            # Preserve input order and vectorize contiguous overwriting renames.
            # LINK publishes a staged exclusive create atomically without replacing
            # an existing destination, including a dangling symlink.
            for exclusive, run in groupby(
                pending, key=lambda item: "x" in item[1].mode
            ):
                if exclusive:
                    for index, file in run:
                        if not file._published:
                            try:
                                fs._client.hardlink(
                                    fs._native_path(file.temp_path),
                                    fs._native_path(file.path),
                                )
                            except (OSError, NotImplementedError) as error:
                                file._label_publication_error(error, index)
                                raise
                            file._published = True
                        file.discard()  # unlink only our staging name and close spool
                else:
                    batch = list(run)
                    try:
                        fs._client.rename_many(
                            [
                                (
                                    fs._native_path(file.temp_path),
                                    fs._native_path(file.path),
                                )
                                for _, file in batch
                            ]
                        )
                    except (OSError, NotImplementedError) as error:
                        position = getattr(error, "index", None)
                        if type(position) is int and 0 <= position < len(batch):
                            index, file = batch[position]
                            file._label_publication_error(error, index)
                        raise
                    # Retire every acknowledged stage before closing any
                    # spool, which can fail and enter transaction cleanup.
                    for _, file in batch:
                        file._prepared = False
                        file._stage_owned = False
                    for _, file in batch:
                        file.discard()

    def commit(self):
        if self._committed:
            return
        self.prepare()
        self.finalize()

    def discard(self):
        primary = None
        try:
            if self._stage_owned:
                try:
                    self._run_scalar(self.fs.rm, self.temp_path)
                except FileNotFoundError:
                    pass
                self._stage_owned = False
                self._prepared = False
        except BaseException as error:
            primary = error
            raise
        finally:
            try:
                self._spool.close()
            except BaseException as cleanup:
                if primary is not None:
                    raise primary from cleanup
                raise
            self._closed = True
        self._committed = True
        self._staging_error = None

    @property
    def closed(self):
        return self._closed


class _VfsiTransaction(Transaction):
    """Stage writes, then publish ordered rename batches and exclusive links."""

    _started = False

    def start(self):
        if self.fs is None:
            raise RuntimeError("transaction has already completed")
        # Enrollment ends during preparation, but the transaction remains
        # active until publication and cleanup finish.
        if self._started or self.fs._intrans:
            raise RuntimeError("a transaction is already active")
        super().start()
        self._started = True

    def __exit__(self, exc_type, exc_value, traceback):
        try:
            return super().__exit__(exc_type, exc_value, traceback)
        except BaseException as cleanup:
            if exc_value is not None:
                raise exc_value from cleanup
            raise

    def complete(self, commit=True):
        files = list(self.files)
        self.files.clear()
        fs = self.fs
        # Enrollment ends before preparation; callbacks cannot enqueue writes
        # into the already-drained transaction.
        if fs is not None:
            fs._intrans = False
        try:
            if not commit:
                _DeferredWriteFile.discard_many(files)
                return
            prepared = []
            try:
                if len(files) > 1 and any("a" in file.mode for file in files):
                    paths = list(dict.fromkeys(file.path for file in files))
                    canonical = {}
                    if len(paths) > 1:
                        canonical, uncertain = fs._canonical_namespace_paths(paths)
                        if uncertain:
                            raise ValueError(
                                "cannot resolve transactional append destination aliases"
                            )
                    appending_by_path = {}
                    for file in files:
                        path = canonical.get(file.path, file.path)
                        appending = "a" in file.mode
                        if path in appending_by_path and (
                            appending or appending_by_path[path]
                        ):
                            raise ValueError(
                                f"transactional append requires a single writer for {file.path!r}"
                            )
                        appending_by_path[path] = appending
                for file in files:
                    file.prepare()
                    prepared.append(file)
                _DeferredWriteFile.finalize_many(prepared)
            except BaseException as error:
                try:
                    _DeferredWriteFile.discard_many(files)
                except BaseException as cleanup_error:
                    raise error from cleanup_error
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
        Export-relative path prefix, e.g. ``"git/vnfs_tests"``. This is not a
        security boundary: server-side symlink resolution can leave the
        prefix. Use a server export rooted at the intended directory for
        isolation.
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
    _default_dir_mode = 0o755

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
        auth=None,
        service_principal=None,
        connection_pool_size=1,
        _mount_config=None,
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
        if auth is None and backend == "nfs":
            raise ValueError(
                "auth must be explicitly set to 'auth_sys', 'krb5', or 'krb5i'"
            )
        if auth is not None and auth not in ("auth_sys", "krb5", "krb5i"):
            raise ValueError("auth must be 'auth_sys', 'krb5', or 'krb5i'")
        if auth is None:
            auth = "auth_sys"
        if backend != "nfs" and (auth != "auth_sys" or service_principal is not None):
            raise ValueError("secure authentication options are NFS-only")
        if service_principal is not None and not isinstance(service_principal, str):
            raise TypeError("service_principal must be a string or None")
        if service_principal is not None and auth == "auth_sys":
            raise ValueError("service_principal requires auth='krb5' or 'krb5i'")
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
        self.auth = auth
        self.service_principal = service_principal
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
        factory_args = (
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
            self.auth,
            self.service_principal,
        )
        disable_native_recovery = backend in ("nfs", "smb") and not self.auto_reconnect
        if _mount_config is not None or disable_native_recovery:
            factory_args += (_mount_config,)
        if disable_native_recovery:
            if (
                "auto_reconnect"
                not in inspect.signature(native_module.NfsClient).parameters
            ):
                adapter = "nfs4fs" if backend == "nfs" else "vsmb"
                raise ImportError(
                    "auto_reconnect=False requires a native adapter with recovery "
                    f"control; upgrade {adapter} alongside vfsi-fsspec"
                )
            factory_args += (False,)
        try:
            bounded_reads = (
                "max_total_bytes"
                in inspect.signature(native_module.NfsClient.read_all_many).parameters
            )
        except (AttributeError, TypeError, ValueError):
            bounded_reads = False
        if not bounded_reads:
            raise ImportError(
                "bounded vector reads require a compatible native adapter; "
                "upgrade nfs4fs or vsmbfs alongside vfsi-fsspec"
            )
        try:
            self._native_has_create_permissions = (
                "permissions"
                in inspect.signature(native_module.NfsClient.open).parameters
            )
        except (AttributeError, TypeError, ValueError):
            self._native_has_create_permissions = False
        self._client = _ClientPool(
            native_module,
            factory_args,
            size=self.connection_pool_size,
            auto_reconnect=self.auto_reconnect,
        )
        self._dircache_lock = threading.RLock()
        self._dircache_generation = self._client.generation
        self._dircache_epoch = 0
        self._dircache_fill_generation = 0
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
        try:
            if not self._client.closed:
                self._client.shutdown()
        finally:
            # _ClientPool.shutdown() invalidates the pool before releasing
            # native sessions. If cleanup then raises, this object is still
            # unusable and must not remain available from fsspec's cache.
            token = getattr(self, "_fs_token_", None)
            cache = type(self)._cache
            if token is not None and cache.get(token) is self:
                cache.pop(token, None)

    def __enter__(self):
        if self.closed:
            raise ValueError("filesystem is closed")
        return self

    def __exit__(self, exc_type, exc_value, traceback):
        _close_reader_on_exit(self, exc_type, exc_value, traceback)

    def start_transaction(self):
        """Begin a write transaction outside a context manager."""
        transaction = self.transaction
        transaction.start()
        return transaction

    def open(
        self,
        path,
        mode="rb",
        block_size=None,
        cache_options=None,
        compression=None,
        **kwargs,
    ):
        # Normalize before compression inference or native setup.
        path = self._checked_strip_protocol(path)
        return _open_with_wrappers(
            self,
            super().open,
            path,
            mode,
            compression=compression,
            block_size=block_size,
            cache_options=cache_options,
            **kwargs,
        )

    def glob(self, path, *args, **kwargs):
        # fsspec checks the trailing separator before stripping the protocol:
        # it means directories only, and must survive path normalization.
        path = _path_string(path)
        directories_only = isinstance(path, str) and path.endswith("/")
        path = self._checked_strip_protocol(path)
        if directories_only and path != "/":
            path += "/"
        return super().glob(path, *args, **kwargs)

    def expand_path(
        self, path, recursive=False, maxdepth=None, assume_literal=False, **kwargs
    ):
        # Older supported fsspec versions lack assume_literal and re-expand
        # matched names containing glob characters, potentially forever.
        if maxdepth is not None and maxdepth < 1:
            raise ValueError("maxdepth must be at least 1")
        paths = [path] if isinstance(path, (str, os.PathLike)) else path
        paths = [self._checked_strip_protocol(item) for item in paths]
        expanded = set()
        for item in paths:
            if not assume_literal and has_magic(item):
                matches = set(self.glob(item, maxdepth=maxdepth, **kwargs))
                expanded.update(matches)
                if not recursive or (maxdepth is not None and maxdepth <= 1):
                    continue
                remaining_depth = None if maxdepth is None else maxdepth - 1
            else:
                matches = [item]
                remaining_depth = maxdepth
            # Glob results are literal paths, even if their names contain
            # brackets, question marks or stars. Reuse bounded find/walk.
            for match in matches:
                if recursive:
                    expanded.update(
                        self.find(
                            match,
                            maxdepth=remaining_depth,
                            withdirs=True,
                            detail=False,
                            **kwargs,
                        )
                    )
                if match not in expanded and (not recursive or self.exists(match)):
                    expanded.add(match)
        if not expanded:
            raise FileNotFoundError(paths)
        return sorted(expanded)

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
        urlpath = _path_string(urlpath)
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
            raise ValueError("explicit URL authority must include a hostname")
        if ":" in host:
            host = f"[{host}]"
        if parsed.port is not None:
            host = f"{host}:{parsed.port}"
        return {"host": host}

    @classmethod
    def _strip_protocol(cls, path):
        if isinstance(path, list):
            return [cls._strip_protocol(p) for p in path]
        path = _path_string(path)
        if not isinstance(path, str):
            path = str(path)
        protos = (cls.protocol,) if isinstance(cls.protocol, str) else cls.protocol
        parsed = urlsplit(path)
        if parsed.scheme in protos:
            if parsed.netloc and (parsed.query or parsed.fragment):
                raise ValueError("query strings and fragments are not supported")
            if not parsed.netloc and "://" in path:
                # Protocol paths produced by _fullpath have an empty authority.
                # Preserve literal query/fragment characters in their filename.
                path = path.split("://", 1)[1]
            else:
                path = parsed.path
            # Unix filenames may contain bytes that are not valid UTF-8.
            path = unquote(path, errors="surrogateescape")
        for protocol in protos:
            if path.startswith(protocol + "://"):
                path = path[len(protocol) + 3 :]
            elif path.startswith(protocol + "::"):
                path = path[len(protocol) + 2 :]
        # The server/share is configured separately; export paths have one
        # root separator. normpath alone preserves exactly two leading slashes.
        path = "/" + path.lstrip("/")
        path = posixpath.normpath(path).rstrip("/")
        return path or cls.root_marker

    def _checked_strip_protocol(self, path):
        """Normalize an instance path without silently changing its server."""
        if isinstance(path, list):
            return [self._checked_strip_protocol(item) for item in path]
        path = _path_string(path)
        if isinstance(path, str):
            requested_host = type(self)._get_kwargs_from_urls(path).get("host")
            if (
                requested_host is not None
                and requested_host.casefold() != self.host.casefold()
            ):
                raise ValueError(
                    f"URL authority {requested_host!r} does not match "
                    f"filesystem host {self.host!r}"
                )
        return type(self)._strip_protocol(path)

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
        # Protocol stripping decodes URL escapes. Quote literal percent
        # signs so metadata names still identify the original backend path.
        return protocol + "://" + internal.replace("%", "%25")

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
        if not self.dircache.use_listings_cache:
            return False
        stored = self._copy_listing(infos)
        with self._dircache_lock:
            if fill_epoch != self._dircache_epoch:
                return False
            self.dircache[internal] = stored
            self._dircache_fill_generation += 1
            return True

    def _canonical_namespace_paths(self, paths, *, follow_final=False):
        """Resolve namespace paths in bounded waves."""
        mapped = {}
        known = {}
        targets = {}
        try:
            pending = [
                (path, native, native if follow_final else posixpath.dirname(native))
                for path in paths
                for native in (self._native_path(path),)
            ]
            for _ in range(40):
                prefixes_by_path = []
                missing = {}
                for _, _, parent in pending:
                    components = parent.strip("/").split("/") if parent != "/" else []
                    prefixes = [
                        "/" + "/".join(components[:i])
                        for i in range(1, len(components) + 1)
                    ]
                    prefixes_by_path.append(prefixes)
                    missing.update(
                        (prefix, None) for prefix in prefixes if prefix not in known
                    )
                missing = list(missing)
                for batch in _bounded_batches(
                    [len(os.fsencode(prefix)) for prefix in missing],
                    self.batch_size,
                    self.max_batch_bytes,
                ):
                    selected = [missing[index] for index in batch]
                    attrs, errors = self._client.lstat_many(selected)
                    if any(
                        _errno(code) not in (errno.ENOENT, errno.ENOTDIR)
                        for code in errors.values()
                    ):
                        return {}, True
                    known.update(zip(selected, attrs))
                unresolved = []
                for (path, native, parent), prefixes in zip(pending, prefixes_by_path):
                    link = next(
                        (
                            prefix
                            for prefix in prefixes
                            if known[prefix] is not None
                            and known[prefix]["type"] == "symlink"
                        ),
                        None,
                    )
                    if link is not None:
                        if link not in targets:
                            target = self._client.readlink(link)
                            parts = target.split("/")
                            if ".." in parts:
                                last_parent = len(parts) - 1 - parts[::-1].index("..")
                                resolved = (
                                    "/"
                                    if target.startswith("/")
                                    else posixpath.dirname(link)
                                )
                                # Before removing a parent step, prove every
                                # intervening name is an ordinary directory.
                                for part in parts[: last_parent + 1]:
                                    if part == "..":
                                        resolved = posixpath.dirname(resolved)
                                    elif part not in ("", "."):
                                        resolved = posixpath.join(resolved, part)
                                        if resolved not in known:
                                            known[resolved] = self._client.lstat(
                                                resolved
                                            )
                                        attrs = known[resolved]
                                        if (
                                            attrs is None
                                            or attrs["type"] != "directory"
                                        ):
                                            return {}, True
                                target = posixpath.join(
                                    resolved, *parts[last_parent + 1 :]
                                )
                            targets[link] = target
                        suffix = parent[len(link) :].lstrip("/")
                        target = targets[link]
                        if follow_final and not suffix and target.endswith(("/", "/.")):
                            # A grouped reader needs a regular file. Normalizing
                            # away the directory requirement can admit a writer
                            # that will create a file the reader cannot open.
                            return {}, True
                        parent = posixpath.normpath(
                            posixpath.join(posixpath.dirname(link), target, suffix)
                        )
                        unresolved.append((path, native, parent))
                        continue
                    canonical = posixpath.normpath(
                        parent
                        if follow_final
                        else posixpath.join(parent, posixpath.basename(native))
                    )
                    if self._root:
                        prefix = "/" + self._root
                        if canonical != prefix and not canonical.startswith(
                            prefix + "/"
                        ):
                            # A server symlink can leave the configured view. Such
                            # names cannot be represented safely as internal paths.
                            return {}, True
                    mapped[path] = self._internalize(canonical)
                pending = unresolved
                if not pending:
                    return mapped, False
        except (OSError, NotImplementedError, ValueError):
            # Cache maintenance must not hide the mutation's actual outcome.
            return {}, True
        return {}, True  # Too many symlinks, including cycles: evict conservatively.

    def _evict_dircache(
        self, subtrees=(), exact=(), identities=(), uncertain=False, namespace=False
    ):
        """Evict many cache keys atomically with one generation bump/scan."""
        subtrees = set(subtrees)
        exact = set(exact)
        with self._dircache_lock:
            self._dircache_epoch += 1
            # SMB may resolve different spellings to the same directory.
            # The client cannot match all server-side aliases lexically.
            if "/" in subtrees or (namespace and self.backend == "smb"):
                self.dircache.clear()
                return
            if not subtrees and not identities and not uncertain:
                for key in exact:
                    self.dircache.pop(key, None)
                return
            keys = list(self.dircache)
            canonical = {}
            if namespace and keys:
                probes = {key: posixpath.join(key, ".vfsi-cache-entry") for key in keys}
                resolved, unknown = self._canonical_namespace_paths(probes.values())
                if unknown:
                    self.dircache.clear()
                    return
                canonical = {
                    key: posixpath.dirname(resolved[probe])
                    for key, probe in probes.items()
                }
            for key in keys:
                if identities or uncertain:
                    infos = self.dircache.get(key, ())
                    if any(
                        uncertain or _handle_identity(info) in identities
                        for info in infos
                    ):
                        exact.add(key)
                in_subtree = False
                for candidate in {key, canonical.get(key, key)}:
                    while candidate != "/":
                        if candidate in subtrees:
                            in_subtree = True
                            break
                        candidate = posixpath.dirname(candidate.rstrip("/")) or "/"
                if key in exact or canonical.get(key) in exact or in_subtree:
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

    def _invalidate_persistent_caches(
        self,
        paths,
        subtrees=False,
        descriptors=None,
        evict_listings=True,
        *,
        follow_final=True,
    ):
        """Invalidate affected paths and every cached alias of their objects."""
        internals = {self._checked_strip_protocol(path) for path in paths}
        if not internals:
            return set(), False
        with self._persistent_cache_lock:
            retained = []
            registered = []
            for cache_ref, invalidator in self._persistent_cache_refs:
                cache = cache_ref()
                if cache is None:
                    continue
                retained.append((cache_ref, invalidator))
                registered.append((cache, invalidator))
            self._persistent_cache_refs = retained
        with self._dircache_lock:
            has_listings = bool(self.dircache)
        if not registered and not has_listings:
            return set(), False
        uncertain = False
        try:
            if descriptors is not None:
                attrs = self._client.fstat_many(descriptors)
                errors = {}
            else:
                stat = (
                    self._client.stat_many if follow_final else self._client.lstat_many
                )
                attrs, errors = stat([self._native_path(path) for path in internals])
            identities = {
                identity
                for attr in attrs
                if attr is not None
                if (identity := _handle_identity(attr)) is not None
            }
            uncertain = any(
                attr is not None and _handle_identity(attr) is None for attr in attrs
            ) or any(
                _errno(code) not in (errno.ENOENT, errno.ENOTDIR)
                for code in errors.values()
            )
        except (OSError, ConnectionError):
            # Metadata permissions/availability must not prevent a permitted
            # write. Conservatively discard cached generations in this case.
            identities = set()
            uncertain = True
        if evict_listings and has_listings:
            self._evict_dircache(identities=identities, uncertain=uncertain)
        for cache, invalidator in registered:
            invalidator(
                cache,
                internals,
                subtrees=subtrees,
                identities=identities,
                uncertain=uncertain,
            )
        return identities, uncertain

    def invalidate_cache(self, path=None):
        """Discard a cached listing and all cached descendants.

        This follows fsspec's public contract for ``path`` while still calling
        the base method so invalidations are replayed after transactions.
        """
        internal = None if path is None else self._checked_strip_protocol(path)
        self._evict_dircache(subtrees={"/" if internal is None else internal})
        super().invalidate_cache(internal)

    def _invalidate_parent_listing(self, path):
        """Discard only the listing containing ``path``."""
        internal = self._checked_strip_protocol(path)
        parent = posixpath.dirname(internal.rstrip("/")) or "/"
        self._evict_dircache(exact={parent})
        # Preserve fsspec's deferred transaction invalidation behavior. Its
        # public invalidation is broader when replayed, which is conservative.
        super().invalidate_cache(parent)

    @contextmanager
    def _cache_mutation(
        self, paths, *, descriptors=None, namespace=False, follow_final=True
    ):
        """Invalidate before mutation and retire any fills made during it."""
        paths = list(paths)
        if not paths:
            yield
            return
        # Include listings completed while the initial invalidation is
        # returning, before the mutation has reached the backend.
        with self._dircache_lock:
            fill_generation = self._dircache_fill_generation
        if namespace:
            self._invalidate_namespace(paths, follow_final=follow_final)
        else:
            self._invalidate_persistent_caches(paths, descriptors=descriptors)
            for path in paths:
                self._invalidate_parent_listing(path)
        primary = None
        try:
            yield
        except BaseException as error:
            primary = error
            raise
        finally:
            try:
                try:
                    if namespace:
                        self._invalidate_namespace(
                            paths, evict_listings=False, follow_final=follow_final
                        )
                    else:
                        with self._persistent_cache_lock:
                            registered = any(
                                ref() is not None
                                for ref, _ in self._persistent_cache_refs
                            )
                        if registered:
                            self._invalidate_persistent_caches(
                                paths, descriptors=descriptors, evict_listings=False
                            )
                finally:
                    with self._dircache_lock:
                        # Reject pending stale listings too. If nothing refilled,
                        # avoid a second scan of unrelated cached directories.
                        self._dircache_epoch += 1
                        if self._dircache_fill_generation != fill_generation:
                            self.dircache.clear()
            except BaseException as cleanup:
                if primary is not None:
                    raise primary from cleanup
                raise

    def _invalidate_namespace(self, paths, evict_listings=True, *, follow_final=True):
        """Invalidate object subtrees, parents, and cached parent metadata."""
        internals = {self._checked_strip_protocol(path) for path in paths}
        if not internals:
            return
        with self._persistent_cache_lock:
            registered = any(
                cache_ref() is not None for cache_ref, _ in self._persistent_cache_refs
            )
        if not registered:
            if not evict_listings:
                return
            if self.backend == "smb":
                # SMB already retires all listings on namespace changes.
                # Metadata probes only matter for persistent cache wrappers.
                self.invalidate_cache()
                return
        if registered or self.dircache:
            canonical, unknown = self._canonical_namespace_paths(internals)
            internals.update(canonical.values())
            if unknown:
                internals.add("/")
        identities, uncertain = self._invalidate_persistent_caches(
            internals,
            subtrees=True,
            evict_listings=False,
            follow_final=follow_final,
        )
        parents = {
            posixpath.dirname(internal.rstrip("/")) or "/" for internal in internals
        }
        # A namespace mutation changes the containing directory's own mtime,
        # change attribute, and sometimes nlink. Those attributes are cached
        # in the listing one level above the containing directory.
        parent_containers = {
            posixpath.dirname(parent.rstrip("/")) or "/" for parent in parents
        }
        if evict_listings:
            self._evict_dircache(
                subtrees=internals,
                exact=parents | parent_containers,
                identities=identities,
                uncertain=uncertain,
                namespace=True,
            )
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
        """Return a complete cached subtree, or None on any cache miss."""
        root_depth = _depth(root)
        requested_depth = None if maxdepth is None else maxdepth - 1
        hard_limit = requested_depth is None or requested_depth > self.walk_max_depth
        limit = None if requested_depth is None else root_depth + requested_depth
        tree = []
        pending = [root]
        budget = _WalkBudget(self, root)
        while pending:
            directory = pending.pop()
            infos = self._cached_listing(directory)
            if infos is None:
                return None
            budget.account(infos)
            tree.append((directory, infos))
            children = []
            for info in infos:
                if info["type"] != "directory":
                    continue
                child = self._checked_strip_protocol(info["name"])
                child_depth = _depth(child)
                if hard_limit and child_depth > root_depth + self.walk_max_depth:
                    raise _oserror(errno.EFBIG, self._fullpath(root))
                if limit is None or child_depth <= limit:
                    children.append(child)
            pending.extend(reversed(children))
        return tree

    def _native_walk_tree(self, internal, fill_epoch, maxdepth=None):
        requested_depth = None if maxdepth is None else maxdepth - 1
        if requested_depth is None:
            native_tree = self._client.walk(self._native_path(internal), sort=True)
        else:
            native_tree = self._client.walk(
                self._native_path(internal),
                sort=True,
                max_depth=min(self.walk_max_depth, requested_depth),
            )
        self._sync_dircache_generation()
        tree = []
        for native_dir, entries in native_tree:
            directory = self._internalize(native_dir)
            infos = self._infos_from_native_entries(entries)
            tree.append((directory, infos))

        root_depth = _depth(internal)
        hard_limit = requested_depth is None or requested_depth > self.walk_max_depth
        if hard_limit and any(
            info["type"] == "directory"
            and _depth(self._checked_strip_protocol(info["name"]))
            > root_depth + self.walk_max_depth
            for _, infos in tree
            for info in infos
        ):
            raise _oserror(errno.EFBIG, self._fullpath(internal))

        for directory, infos in tree:
            self._store_listing(directory, infos, fill_epoch)
        return tree

    def _prefetch_walk_children(self, children, on_error, budget):
        """Retain bounded sibling listings until visited, independently of TTL cache."""
        if not children or on_error == "raise":
            return
        uncached = [child for child in children if self._cached_listing(child) is None]
        if not uncached:
            return
        for start in range(0, len(uncached), self.batch_size):
            cohort = uncached[start : start + self.batch_size]
            budget.sync()
            fill_epoch = budget.epoch
            try:
                groups = self._client.listdir_many(
                    [self._native_path(child) for child in cohort],
                    recursive=False,
                    **budget.native_options(),
                )
            except OSError as error:
                # Normal traversal reports errors in visit order. Leave an
                # over-budget cohort for scalar visits with the actual budget.
                if error.errno == errno.EFBIG:
                    return
                continue
            budget.sync()
            if fill_epoch != budget.epoch:
                # Namespace changes or reconnects while fetching invalidate
                # these results too. Normal traversal will fetch them afresh.
                continue
            for child, entries in zip(cohort, groups):
                infos = self._infos_from_native_entries(entries)
                if not budget.account(infos, prefetch=True):
                    return
                budget.prefetched[child] = infos
                self._store_listing(child, infos, fill_epoch)

    def _best_effort_walk_tree(self, root, maxdepth, on_error):
        """Walk one directory at a time after a failed native traversal."""
        root_depth = _depth(root)
        requested_depth = None if maxdepth is None else maxdepth - 1
        depth_limit = (
            self.walk_max_depth
            if requested_depth is None
            else min(self.walk_max_depth, requested_depth)
        )
        max_entry_depth = root_depth + depth_limit
        hard_limit = requested_depth is None or requested_depth > self.walk_max_depth
        tree = []
        pending = [root]
        budget = _WalkBudget(self, root)
        while pending:
            directory = pending.pop()
            try:
                infos = self.ls(
                    directory, detail=True, refresh=True, _walk_budget=budget
                )
            except (FileNotFoundError, OSError) as error:
                if getattr(error, "errno", None) == errno.EFBIG:
                    raise
                if on_error == "raise":
                    raise
                if callable(on_error):
                    on_error(error)
                continue
            budget.account(infos)
            tree.append((directory, infos))
            children = [
                self._checked_strip_protocol(info["name"])
                for info in infos
                if info["type"] == "directory"
            ]
            if _depth(directory) >= max_entry_depth:
                if hard_limit and children:
                    raise _oserror(errno.EFBIG, self._fullpath(root))
                continue
            pending.extend(reversed(children))
        return tree

    # -- batched directory creation ---------------------------------------

    def _rebase_directory_error(self, error, prefixes, paths):
        """Attribute an implicit parent only when it serves one requested path."""
        position = getattr(error, "index", None)
        if (
            isinstance(position, int)
            and not isinstance(position, bool)
            and 0 <= position < len(prefixes)
        ):
            prefix = prefixes[position]
            matches = [
                index
                for index, path in enumerate(paths)
                if path == prefix or path.startswith(prefix + "/")
            ]
            _oserror(error, self._internalize(prefix))
            if len(matches) == 1:
                error.index = matches[0]
            else:
                del error.index

    def _ensure_dirs(
        self, paths, mode=None, *, verify_existing=False, parents_only=False
    ):
        """``mkdir -p`` a set of internal paths with as few compounds as
        possible: one batched existence probe, then one mkdirv batch per
        missing depth level."""
        mode = self._default_dir_mode if mode is None else mode
        paths = list(dict.fromkeys(p for p in paths if p not in ("",)))
        if not paths:
            return
        native_paths = [self._native_path(p) for p in paths]
        prefixes = set()
        for native in native_paths:
            if parents_only:
                native = posixpath.dirname(native)
            comps = [c for c in native.split("/") if c]
            prefixes.update("/" + "/".join(comps[:i]) for i in range(1, len(comps) + 1))
        ordered = sorted(prefixes, key=lambda p: (p.count("/"), p))
        if not ordered:
            return
        try:
            exists = self._client.exists_many(list(ordered))
        except Exception as error:
            self._rebase_directory_error(error, ordered, native_paths)
            raise
        missing = [p for p, ok in zip(ordered, exists) if not ok]
        if verify_existing:
            # The caller observed missing leaves before this probe. A name
            # that appeared since then must resolve to a directory.
            missing_paths = set(missing)
            appeared = [path for path in native_paths if path not in missing_paths]
            if appeared:
                try:
                    stats, errors = self._client.stat_many(appeared)
                except Exception as error:
                    self._rebase_directory_error(error, appeared, native_paths)
                    raise
                for index, (native, attrs) in enumerate(zip(appeared, stats)):
                    if index in errors and _errno(errors[index]) == errno.ENOENT:
                        missing.append(native)
                        continue
                    if index in errors or attrs is None or attrs["type"] != "directory":
                        cause = errors.get(
                            index, errno.EIO if attrs is None else errno.EEXIST
                        )
                        error = _oserror(cause, self._internalize(native), index=index)
                        self._rebase_directory_error(error, appeared, native_paths)
                        raise error
        by_depth = {}
        for p in missing:
            by_depth.setdefault(p.count("/"), []).append(p)
        internal_missing = [
            internal
            for native in missing
            if self._native_path(internal := self._internalize(native)) == native
        ]
        with self._cache_mutation(internal_missing, namespace=True):
            for depth in sorted(by_depth):
                pending = by_depth[depth]
                while pending:
                    try:
                        self._client.mkdir_many(pending, mode)
                        break
                    except FileExistsError as collision:
                        self._rebase_directory_error(collision, pending, native_paths)
                        # A concurrent mkdir-p may have won after exists_many.
                        # Accept only paths that now resolve to directories and
                        # retry siblings that remain absent.
                        try:
                            stats, errors = self._client.stat_many(pending)
                        except Exception as error:
                            self._rebase_directory_error(error, pending, native_paths)
                            raise
                        missing = []
                        found_directory = False
                        for index, native in enumerate(pending):
                            if index in errors:
                                if _errno(errors[index]) == errno.ENOENT:
                                    missing.append(native)
                                    continue
                                error = _oserror(
                                    errors[index],
                                    self._internalize(native),
                                    index=index,
                                )
                                self._rebase_directory_error(
                                    error, pending, native_paths
                                )
                                raise error from None
                            attrs = stats[index]
                            if attrs is None or attrs["type"] != "directory":
                                raise
                            found_directory = True
                        if not missing:
                            break
                        if not found_directory:
                            raise
                        pending = missing
                    except Exception as error:
                        self._rebase_directory_error(error, pending, native_paths)
                        raise

    def _ensure_file_parents(self, paths):
        parents = list(
            dict.fromkeys(
                posixpath.dirname(path)
                for path in paths
                if posixpath.dirname(path) not in ("", "/")
            )
        )
        try:
            self._ensure_dirs(parents)
        except Exception as error:
            self._rebase_directory_error(error, parents, paths)
            raise

    def _makedirs_batched(self, paths, exist_ok=False):
        """fsspec ``makedirs`` semantics for many paths, batched."""
        paths = list(dict.fromkeys(self._checked_strip_protocol(p) for p in paths))
        if not paths:
            return
        native = [self._native_path(p) for p in paths]
        try:
            lstats, lerrs = self._client.lstat_many(native)
        except Exception as error:
            _rebase_error(error, range(len(paths)), paths)
            raise
        missing = []
        existing = []
        for i, p in enumerate(paths):
            if lstats[i] is not None:
                existing.append((i, p))
            else:
                code = lerrs.get(i)
                if code is not None and _errno(code) != errno.ENOENT:
                    raise _oserror(code, p, index=i)
                missing.append(p)
        if existing and not exist_ok:
            raise FileExistsError(errno.EEXIST, f"File exists: {existing[0][1]!r}")
        if existing:
            try:
                estats, eerrs = self._client.stat_many(
                    [self._native_path(p) for _, p in existing]
                )
            except Exception as error:
                _rebase_error(
                    error,
                    [index for index, _ in existing],
                    [path for _, path in existing],
                )
                raise
            for j, (i, p) in enumerate(existing):
                if estats[j] is not None and estats[j]["type"] == "directory":
                    continue
                if j in eerrs and _errno(eerrs[j]) != errno.ENOENT:
                    raise _oserror(eerrs[j], p, index=i)
                raise FileExistsError(errno.EEXIST, f"File exists: {p!r}")
        if missing:
            try:
                self._ensure_dirs(missing, verify_existing=True)
            except Exception as error:
                positions = {path: index for index, path in enumerate(paths)}
                _rebase_error(error, [positions[path] for path in missing], missing)
                raise

    # -- metadata ----------------------------------------------------------

    def info(self, path, **kwargs):
        internal = self._checked_strip_protocol(path)
        stats, errors = self._client.stat_many([self._native_path(internal)])
        if errors:
            raise _oserror(errors[0], internal)
        return _info_dict(self._fullpath(internal), stats[0])

    def _info_many(self, paths):
        """Return metadata for several paths in one vector operation."""
        internals = [self._checked_strip_protocol(path) for path in paths]
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
        internal = self._checked_strip_protocol(path)
        budget = kwargs.pop("_walk_budget", None)
        if budget is not None and internal in budget.prefetched:
            infos = budget.prefetched.pop(internal)
            return (
                self._copy_listing(infos)
                if detail
                else [info["name"] for info in infos]
            )
        if not refresh:
            cached = self._cached_listing(internal)
            if cached is not None:
                if detail:
                    return cached
                return [info["name"] for info in cached]
        else:
            self._evict_dircache(exact={internal})
        fill_epoch = self._dircache_epoch_snapshot()
        native_path = self._native_path(internal)
        try:
            entries = (
                self._client.listdir(native_path)
                if budget is None
                else budget.listdir(native_path)
            )
        except NotADirectoryError:
            # The path is a file (or symlink to one): report it directly.
            info = self.info(internal, **kwargs)
            return [info] if detail else [info["name"]]
        infos = self._infos_from_native_entries(entries)
        self._store_listing(internal, infos, fill_epoch)
        if detail:
            return infos
        return [d["name"] for d in infos]

    def exists(self, path, **kwargs):
        try:
            internal = self._checked_strip_protocol(path)
            return self._client.exists_many([self._native_path(internal)])[0]
        except (FileNotFoundError, NotADirectoryError):
            return False

    def isfile(self, path):
        try:
            return self.info(path)["type"] == "file"
        except (FileNotFoundError, NotADirectoryError):
            return False

    def _directory_info(self, path):
        try:
            info = self.info(path)
        except (FileNotFoundError, NotADirectoryError):
            return None
        return info if info["type"] == "directory" else None

    def isdir(self, path):
        return self._directory_info(path) is not None

    def size(self, path):
        return self.info(path).get("size")

    def sizes(self, paths):
        """Size of each path in one stat_many batch."""
        internals = [self._checked_strip_protocol(p) for p in paths]
        stats, errors = self._client.stat_many(
            [self._native_path(p) for p in internals]
        )
        out = []
        for i, p in enumerate(internals):
            if stats[i] is None:
                raise _oserror(errors[i], p)
            out.append(stats[i].get("size"))
        return out

    def created(self, path):
        return _timestamp_datetime(self.info(path), "created")

    def modified(self, path):
        return _timestamp_datetime(self.info(path), "modified")

    def ukey(self, path):
        return self._ukey_from_info(self.info(path))

    @classmethod
    def _ukey_from_info(cls, info):
        # Like LocalFileSystem (a hash of `info`, which includes the name),
        # the key changes when the file is renamed as well as when its
        # contents change.
        return hashlib.sha256(
            f"{info.get('name')}:{cls._version_token(info)}".encode(
                "utf-8", "surrogateescape"
            )
        ).hexdigest()

    @staticmethod
    def _version_token(info):
        """Best available identity for one version of a filesystem object."""
        change = info.get("change")
        if change is not None:
            return (
                f"fs_id:{info.get('fs_id')}:change:{change}:fileid:{info.get('fileid')}"
            )
        return (
            f"fs_id:{info.get('fs_id')}:fileid:{info.get('fileid')}:"
            f"mtime_ns:{info.get('modified_ns')}:"
            f"ctime_ns:{info.get('created_ns')}:size:{info.get('size')}"
        )

    def checksum(self, path):
        # A stable file-version value. NFS FATTR4_CHANGE is preferred; the
        # nanosecond metadata fallback covers local/SMB backends.
        info = self.info(path)
        digest = hashlib.sha256(self._version_token(info).encode()).hexdigest()
        return int(digest, 16)

    # -- bulk read / write -------------------------------------------------

    def _cat_batch(
        self, paths, on_error, callback=DEFAULT_CALLBACK, start=None, end=None
    ):
        """Read path groups bounded by both item count and aggregate bytes."""
        if start is not None or end is not None:
            values = self.cat_ranges(
                paths, start, end, on_error="return", callback=callback
            )
            out = {}
            for path, value in zip(paths, values):
                if isinstance(value, Exception):
                    if on_error == "raise":
                        raise value
                    if on_error == "return":
                        out[path] = value
                else:
                    out[path] = value
            return out
        callback.set_size(len(paths))
        native = [self._native_path(path) for path in paths]
        stats, stat_errors = self._client.stat_many(native)
        data = [b""] * len(paths)
        failures = {
            index: _oserror(code, paths[index], index=index)
            for index, code in stat_errors.items()
        }
        reported = set()
        valid = [index for index in range(len(paths)) if index not in failures]
        valid_sizes = [stats[index].get("size", 0) for index in valid]
        limit = self.read_all_max_total_bytes
        retained = 0

        def read_streamed(index, size, budget, *, growth_fallback=False):
            nonlocal retained
            # Callback setup/exit are application code, not per-file I/O errors.
            with callback.branched(paths[index], _MEMORY_PATH) as child:
                if size is not None:
                    child.set_size(size)
                callback_error = None

                def progress(written):
                    nonlocal callback_error
                    try:
                        child.relative_update(written)
                    except OSError as error:
                        callback_error = error
                        raise

                try:
                    value = self._read_one_streamed(
                        paths[index],
                        on_progress=progress,
                        max_bytes=budget,
                        allocation_base=retained,
                        allocation_limit=limit,
                    )
                    if retained + len(value) > limit:
                        raise _allocation_error(
                            paths[index], retained + len(value), limit
                        )
                except OSError as exc:
                    if exc is callback_error:
                        raise
                    error = _oserror(exc, paths[index], index=index)
                    if growth_fallback and _is_transport_error(error):
                        raise error
                    failures[index] = error
                else:
                    if size != len(value):
                        child.set_size(len(value))
                    data[index] = value
                    retained += len(value)
            callback.relative_update()
            reported.add(index)

        pending = deque(zip(valid, valid_sizes))
        while pending:
            remaining = limit - retained

            batch = []
            estimated = 0
            oversized = False
            byte_limit = min(self.max_batch_bytes, remaining)
            while pending and len(batch) < self.batch_size:
                index, size = pending[0]
                if batch and estimated + size > byte_limit:
                    break
                if not batch and (size > self.max_batch_bytes or size > remaining):
                    pending.popleft()
                    batch = [(index, size)]
                    oversized = True
                    break
                pending.popleft()
                batch.append((index, size))
                estimated += size
            if not batch:
                continue

            if oversized:
                index, size = batch[0]
                read_streamed(index, size, remaining)
                continue

            # Stat sizes are planning hints, not permission checks or EOF.
            # A zero remaining budget still permits the native bounded EOF
            # probe, so a formerly empty file cannot silently bypass the cap.
            try:
                dat, batch_errors = self._client.read_all_many(
                    [native[index] for index, _ in batch], byte_limit
                )
            except Exception as error:
                _rebase_error(
                    error,
                    [index for index, _ in batch],
                    [paths[index] for index, _ in batch],
                )
                raise
            grew = []
            for position, (index, _) in enumerate(batch):
                if position in batch_errors:
                    error = _oserror(batch_errors[position], paths[index], index=index)
                    if byte_limit < remaining and _errno(error) == errno.EFBIG:
                        grew.append(index)
                    else:
                        failures[index] = error
                    continue
                value = dat[position] or b""
                if retained + len(value) > limit:
                    failures[index] = _allocation_error(
                        paths[index], retained + len(value), limit
                    )
                    continue
                data[index] = value
                retained += len(value)
            # Reserve the admitted siblings before streaming files that
            # outgrew the per-batch estimate.
            del dat
            for index in grew:
                read_streamed(index, None, limit - retained, growth_fallback=True)

        out = {}
        for i, path in enumerate(paths):
            if i not in reported:
                if i in failures:
                    callback.relative_update()
                else:
                    _complete_child(callback, path, _MEMORY_PATH, len(data[i]))
            if i in failures:
                exc = failures[i]
                if on_error == "raise":
                    raise exc
                if on_error == "return":
                    out[path] = exc
                # "omit": skip the failed key
            else:
                out[path] = data[i]
        return out

    def _read_one_streamed(
        self,
        path,
        on_progress=None,
        max_bytes=None,
        allocation_base=0,
        allocation_limit=None,
    ):
        """Read one result incrementally within the public allocation cap."""
        output = io.BytesIO()
        self._copy_remote_to_fileobj(
            path,
            output,
            on_progress=on_progress,
            max_bytes=max_bytes,
            allocation_base=allocation_base,
            allocation_limit=allocation_limit,
        )
        return output.getvalue()

    def cat(
        self,
        path,
        recursive=False,
        on_error="raise",
        callback=DEFAULT_CALLBACK,
        **kwargs,
    ):
        path = _path_string(path)
        callback = Callback.as_callback(callback)
        if isinstance(path, str):
            paths = self.expand_path(path, recursive=recursive, **kwargs)
            if len(paths) == 1 and paths[0] == self._checked_strip_protocol(path):
                # Single literal path: cat_file semantics (raise on error).
                return self.cat_file(paths[0], callback=callback, **kwargs)
            return self._cat_batch(
                paths,
                on_error,
                callback=callback,
                start=kwargs.get("start"),
                end=kwargs.get("end"),
            )
        internals = [self._checked_strip_protocol(entry) for entry in path]
        # Expand the whole selection together: an unmatched glob is not a
        # missing literal, and overlapping selections must consume the read
        # budget and report progress only once per canonical path.
        literal = kwargs.get("assume_literal", False) or not any(
            has_magic(entry) for entry in internals
        )
        if recursive and literal:
            # find already selects files from directory-entry metadata; do not
            # add a scalar stat per descendant just to exclude directories.
            paths = sorted(
                {
                    p
                    for entry in internals
                    for p in self.find(entry, withdirs=False, **kwargs)
                }
            )
        elif literal:
            # Preserve caller order and the no-expansion fast path for literal
            # vectors while deduplicating canonical aliases.
            paths = list(dict.fromkeys(internals))
        else:
            paths = (
                self.expand_path(internals, recursive=recursive, **kwargs)
                if internals
                else []
            )
            if recursive:
                paths = [p for p in paths if not self.isdir(p)]
        return self._cat_batch(
            paths,
            on_error,
            callback=callback,
            start=kwargs.get("start"),
            end=kwargs.get("end"),
        )

    def cat_file(self, path, start=None, end=None, callback=DEFAULT_CALLBACK, **kwargs):
        callback = Callback.as_callback(callback)
        internal = self._checked_strip_protocol(path)
        start, end = _range_indices(start, end)
        size = None
        if start < 0 or (end is not None and end < 0):
            size = _range_file_size(self.info(internal), internal)
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
            data, errors = self._client.read_all_many(
                [self._native_path(internal)], self.read_all_max_total_bytes
            )
            if errors:
                raise _oserror(errors[0], internal)
            result = data[0] or b""
            _complete_callback(callback, len(result))
            return result
        if end is None:
            if size is None:
                size = _range_file_size(self.info(internal), internal)
            # Freeze EOF at the checked size; a second native size query
            # could otherwise enlarge this allocation after concurrent growth.
            end = max(start, size)
        self._check_read_allocation(max(0, end - start), internal)
        _check_range_offsets(start, end)
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
        internals = [self._checked_strip_protocol(p) for p in paths]
        native = [self._native_path(p) for p in internals]
        errors = {}
        validation_errors = {}
        normalized = []
        return_errors = on_error == "return"
        sizes = [None] * len(paths)
        size_indices = []
        for index, (start, end) in enumerate(zip(starts, ends)):
            try:
                start, end = _range_indices(start, end)
            except (TypeError, ValueError, OverflowError) as error:
                validation_errors[index] = _oserror(
                    error, internals[index], index=index
                )
                normalized.append((0, 0))
                continue
            normalized.append((start, end))
            if start < 0 or end is None or end < 0 or end == start - 1:
                size_indices.append(index)
        if validation_errors and not return_errors:
            raise validation_errors[min(validation_errors)]
        if size_indices:
            # Explicit bounded ranges do not depend on metadata permissions.
            try:
                stats, stat_errors = self._client.stat_many(
                    [native[index] for index in size_indices]
                )
            except Exception as error:
                _rebase_error(
                    error, size_indices, [internals[index] for index in size_indices]
                )
                raise
            errors.update(
                (size_indices[position], error)
                for position, error in stat_errors.items()
            )
            for index, stat in zip(size_indices, stats):
                if index in errors:
                    continue
                try:
                    sizes[index] = _range_file_size(stat, internals[index])
                except OSError as error:
                    errors[index] = error
        for i, (start, end) in enumerate(normalized):
            if i in errors or i in validation_errors:
                continue
            try:
                start, end = _normalize_range(start, end, sizes[i])
                # Use the same EOF for budgeting and execution, including
                # fsspec's end=start-1 spelling of an open-ended read.
                end = max(start, sizes[i]) if end is None else end
                if return_errors:
                    _check_range_offsets(start, end)
                normalized[i] = (start, end)
            except OverflowError as exc:
                validation_errors[i] = _oserror(exc, internals[i], index=i)
            except ValueError as exc:
                try:
                    info = self.info(internals[i])
                    if info["type"] == "directory":
                        raise IsADirectoryError(
                            errno.EISDIR, "Is a directory", internals[i]
                        )
                except (OSError, ValueError) as path_error:
                    _rebase_error(path_error, [i], [internals[i]])
                    validation_errors[i] = path_error
                else:
                    validation_errors[i] = exc
        if validation_errors and not return_errors:
            raise validation_errors[min(validation_errors)]
        valid = [
            i
            for i in range(len(paths))
            if i not in errors and i not in validation_errors
        ]
        data = [None] * len(paths)
        lengths = [normalized[i][1] - normalized[i][0] for i in valid]
        self._check_read_allocation(
            sum(lengths), internals[valid[0]] if valid else None
        )
        if not return_errors:
            for i in valid:
                try:
                    _check_range_offsets(*normalized[i])
                except OverflowError as exc:
                    raise _oserror(exc, internals[i], index=i)
        for positions in _bounded_batches(
            lengths, self.batch_size, self.max_batch_bytes
        ):
            batch = [valid[position] for position in positions]
            try:
                valid_data, read_errors = self._client.read_many(
                    [native[i] for i in batch],
                    [normalized[i][0] for i in batch],
                    [normalized[i][1] for i in batch],
                )
            except Exception as error:
                _rebase_error(error, batch, [internals[index] for index in batch])
                raise
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
                exc = _oserror(errors[i], p, index=i)
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
        input_indices=None,
    ):
        _validate_write_mode(mode)
        with self._cache_mutation(paths, namespace=True):
            native = [self._native_path(p) for p in paths]
            datas = [
                v if isinstance(v, bytes) else bytes(memoryview(v)) for v in values
            ]
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

            if self._intrans:
                for index, path in enumerate(paths):
                    report(
                        index,
                        lambda child, i=index, p=path: self._write_one_streamed(
                            p,
                            datas[i],
                            callback=child,
                            mode="xb" if mode == "create" else "wb",
                            input_index=(
                                i if input_indices is None else input_indices[i]
                            ),
                        ),
                    )
                return
            if mode == "create":
                for index, path in enumerate(paths):
                    report(
                        index,
                        lambda child, i=index, p=path: self._write_one_exclusive(
                            p,
                            datas[i],
                            callback=child,
                            input_index=(
                                i if input_indices is None else input_indices[i]
                            ),
                        ),
                    )
                return
            for batch in _bounded_batches(
                [len(data) for data in datas], self.batch_size, self.max_batch_bytes
            ):
                if len(batch) == 1 and len(datas[batch[0]]) > self.max_batch_bytes:
                    index = batch[0]
                    report(
                        index,
                        lambda child, i=index: self._write_one_streamed(
                            paths[i],
                            datas[i],
                            callback=child,
                            input_index=(
                                i if input_indices is None else input_indices[i]
                            ),
                        ),
                    )
                    continue
                batch_native = [native[i] for i in batch]
                batch_datas = [datas[i] for i in batch]
                # Parent preflight errors use the same batch positions as WRITE.
                # Never replay writes, even for an indexed ENOENT.
                try:
                    if self.auto_mkdir:
                        self._ensure_file_parents([paths[i] for i in batch])
                    written = self._client.write_many(
                        batch_native, batch_datas, truncate=True
                    )
                except Exception as error:
                    _rebase_error(
                        error,
                        (
                            batch
                            if input_indices is None
                            else [input_indices[index] for index in batch]
                        ),
                        [paths[index] for index in batch],
                    )
                    raise
                for position, index in enumerate(batch):
                    expected = len(datas[index])
                    if written[position] != expected:
                        raise _oserror(
                            OSError(
                                errno.EIO,
                                f"short write: wrote {written[position]} of {expected} bytes",
                                paths[index],
                            ),
                            paths[index],
                            index=(
                                index if input_indices is None else input_indices[index]
                            ),
                        )
                for index in batch:
                    report(index)

    @contextmanager
    def _rollback_transaction_writers(self):
        transaction = self._transaction if self._intrans else None
        opened = []

        def open_tracked(opener, *args, **kwargs):
            if transaction is None:
                return opener(*args, **kwargs)
            token = _OPERATION_WRITERS.set((self, opened))
            try:
                return opener(*args, **kwargs)
            finally:
                _OPERATION_WRITERS.reset(token)

        try:
            yield open_tracked
        except BaseException as primary:
            for file in opened:
                if file in transaction.files:
                    transaction.files.remove(file)
            try:
                _DeferredWriteFile.discard_many(opened)
            except BaseException as cleanup:
                raise primary from cleanup
            raise

    def _write_one_streamed(
        self, path, value, callback=None, mode="wb", *, input_index=None
    ):
        """Write one in-memory value through the existing scalar/deferred path."""
        view = memoryview(value)
        on_progress = callback.relative_update if callback is not None else None
        with self._rollback_transaction_writers() as open_tracked, ExitStack() as stack:
            try:
                remote = open_tracked(self.open, path, mode)
            except Exception as error:
                if input_index is not None:
                    _rebase_error(error, [input_index], [path])
                raise
            stack.push(partial(_close_reader_on_exit, remote))
            for offset in range(0, len(view), self.transfer_chunk_size):
                _write_fully(
                    remote,
                    bytes(view[offset : offset + self.transfer_chunk_size]),
                    on_progress=on_progress,
                )

    def _write_one_exclusive(self, path, value, callback=None, *, input_index=None):
        """Atomically create one path with O_EXCL and bounded writes."""
        internal = self._checked_strip_protocol(path)
        parent = posixpath.dirname(internal)
        if self.auto_mkdir and parent not in ("", "/"):
            try:
                self._ensure_dirs([parent])
            except Exception as error:
                if input_index is not None:
                    _rebase_error(error, [input_index], [internal])
                raise
        fd = self._client.open(self._native_path(internal), "xb")
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
        except BaseException as exc:
            write_error = exc
            raise
        finally:
            try:
                self._client._close_live_token(fd)
            except BaseException:
                # This helper has no file object to retain cleanup ownership.
                self._client.defer_close_many([fd])
                # Preserve the mutation error if both the write and CLOSE fail.
                if write_error is None:
                    raise
            # The path may now name another writer's file. Retain partial
            # output on failure rather than unlinking a name we no longer own.

    def _copy_remote_to_fileobj(
        self,
        path,
        output,
        on_progress=None,
        max_bytes=None,
        allocation_base=0,
        allocation_limit=None,
        *,
        source=None,
    ):
        """Stream one remote file into a writable local file object."""
        streamed = 0
        with ExitStack() as stack:
            remote = source
            if remote is None:
                remote = self.open(path, "rb", cache_type="none")
                stack.push(partial(_close_reader_on_exit, remote))
            if max_bytes == 0 or (
                max_bytes is None and self.read_all_max_total_bytes == 0
            ):
                limit = (
                    self.read_all_max_total_bytes
                    if allocation_limit is None
                    else allocation_limit
                )
                _require_empty_read(remote, path, limit, allocation_base)
                return
            while True:
                request = self._stream_read_size()
                if max_bytes is not None:
                    request = min(request, max_bytes - streamed + 1)
                chunk = remote.read(request)
                if not chunk:
                    break
                if max_bytes is not None and streamed + len(chunk) > max_bytes:
                    limit = max_bytes if allocation_limit is None else allocation_limit
                    raise _allocation_error(
                        path, allocation_base + streamed + len(chunk), limit
                    )
                _write_fully(output, chunk, on_progress=on_progress, none_is_full=True)
                streamed += len(chunk)

    def pipe(self, path, value=None, callback=DEFAULT_CALLBACK, **kwargs):
        path = _path_string(path)
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
            paths = [self._checked_strip_protocol(k) for k in path]
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
            [self._checked_strip_protocol(path)], [value], mode, callback=callback
        )

    # -- batched local <-> remote transfer --------------------------------

    def _open_transfer_file(self, path, mode, input_index, **open_kwargs):
        def open_native(path, native_mode, **kwargs):
            try:
                file = self.open(path, native_mode, _defer_cache=True, **kwargs)
            except Exception as error:
                if (
                    isinstance(error, OSError)
                    and getattr(error, "index", None) is None
                    and not _is_transport_error(error)
                ):
                    _oserror(error, path, index=input_index)
                else:
                    _rebase_error(error, [input_index], [path])
                raise
            # Configurable cache factories are application code, like codecs.
            if isinstance(file, VfsiFile):
                try:
                    file._initialize_read_cache()
                except BaseException as error:
                    _close_reader_on_exit(file, type(error), error, error.__traceback__)
                    raise
            return file

        # Codec construction is application code; only the bare OPEN is indexed.
        return _open_with_wrappers(self, open_native, path, mode, **open_kwargs)

    def _download_file_streamed(
        self,
        internal,
        lpath,
        callback,
        *,
        outfile=None,
        input_index=None,
        source_size=None,
        **open_kwargs,
    ):
        from fsspec.implementations.local import LocalFileSystem

        open_kwargs.setdefault("cache_type", "none")
        with ExitStack() as stack:
            if input_index is None:
                remote = self.open(internal, "rb", **open_kwargs)
            else:
                remote = self._open_transfer_file(
                    internal, "rb", input_index, **open_kwargs
                )
            stack.push(partial(_close_reader_on_exit, remote))
            if source_size is None:
                source_size = getattr(remote, "size", None)
            callback.set_size(source_size)
            empty_budget = self.read_all_max_total_bytes == 0
            if empty_budget:
                _require_empty_read(remote, internal, 0)
            if outfile is None:
                local = LocalFileSystem(auto_mkdir=True)
                local.makedirs(local._parent(lpath), exist_ok=True)
                outfile = stack.enter_context(open(lpath, "wb"))
            if not empty_budget:
                self._copy_remote_to_fileobj(
                    internal,
                    outfile,
                    on_progress=callback.relative_update,
                    source=remote,
                )

    def get_file(
        self, rpath, lpath=None, callback=DEFAULT_CALLBACK, outfile=None, **kwargs
    ):
        """Download with bounded reads, complete writes, and owned cleanup."""
        from fsspec.utils import isfilelike

        internal = self._checked_strip_protocol(rpath)
        callback = Callback.as_callback(callback)
        if outfile is None and isfilelike(lpath):
            outfile = lpath
        elif outfile is None and self.isdir(internal):
            os.makedirs(lpath, exist_ok=True)
            return
        self._download_file_streamed(
            internal, lpath, callback, outfile=outfile, **kwargs
        )

    def get(
        self,
        rpath,
        lpath,
        recursive=False,
        callback=DEFAULT_CALLBACK,
        maxdepth=None,
        **kwargs,
    ):
        """Batch plain downloads; stream files with explicit open options."""
        callback = Callback.as_callback(callback)
        if kwargs:
            for option in (
                "compression",
                "block_size",
                "cache_type",
                "cache_options",
                "write_buffering",
                "size",
            ):
                if kwargs.get(option, True) is None:
                    kwargs.pop(option)
        from fsspec.implementations.local import (
            LocalFileSystem,
            make_path_posix,
            trailing_sep,
        )
        from fsspec.utils import other_paths

        lpath, rpath = _path_string(lpath), _path_string(rpath)
        pre_stats = None
        if not isinstance(lpath, str) and not isinstance(rpath, str):
            lpath, rpath = list(lpath), list(rpath)
            if len(rpath) != len(lpath):
                raise ValueError("rpath and lpath must have equal length")
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
                    [
                        self._native_path(self._checked_strip_protocol(p))
                        for p in candidates
                    ]
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

        # Expansion has consumed these traversal controls; none configure OPEN.
        for option in ("assume_literal", "on_error", "refresh", "topdown"):
            kwargs.pop(option, None)
        callback.set_size(len(lpaths))
        local = LocalFileSystem(auto_mkdir=True)
        if pre_stats is not None:
            stats = [pre_stats.get(r) for r in rpaths]
        else:
            # Classify directories with one stat_many batch.
            native_all = [
                self._native_path(self._checked_strip_protocol(r)) for r in rpaths
            ]
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
                        stats[i].get("size", 0) if stats[i] else 0,
                        i,
                    )
                )
        if not pairs:
            return
        if kwargs:
            for remote_path, local_path, _, public_index in pairs:
                with callback.branched(remote_path, local_path) as child:
                    self._download_file_streamed(
                        self._checked_strip_protocol(remote_path),
                        local_path,
                        child,
                        input_index=public_index,
                        **kwargs,
                    )
                callback.relative_update()
            return
        read_batch_bytes = min(self.max_batch_bytes, self.read_all_max_total_bytes)
        for batch in _bounded_batches(
            [size for _, _, size, _ in pairs], self.batch_size, read_batch_bytes
        ):
            if len(batch) == 1 and pairs[batch[0]][2] > read_batch_bytes:
                remote_path, local_path, size, public_index = pairs[batch[0]]
                with callback.branched(remote_path, local_path) as child:
                    self._download_file_streamed(
                        self._checked_strip_protocol(remote_path),
                        local_path,
                        child,
                        input_index=public_index,
                        source_size=size,
                    )
                callback.relative_update()
                continue
            selected = [pairs[i] for i in batch]
            native = [
                self._native_path(self._checked_strip_protocol(remote_path))
                for remote_path, _, _, _ in selected
            ]
            try:
                data, errors = self._client.read_all_many(native, read_batch_bytes)
            except Exception as error:
                _rebase_error(
                    error,
                    [public_index for _, _, _, public_index in selected],
                    [remote_path for remote_path, _, _, _ in selected],
                )
                raise
            for i, ((remote_path, local_path, _, public_index), buf) in enumerate(
                zip(selected, data)
            ):
                with callback.branched(remote_path, local_path) as child:
                    if buf is None:
                        error = _oserror(errors[i], remote_path, index=public_index)
                        if _errno(error) != errno.EFBIG or read_batch_bytes == 0:
                            raise error
                        # The source outgrew the batching threshold. Stream
                        # this input without replaying buffered siblings.
                        self._download_file_streamed(
                            self._checked_strip_protocol(remote_path),
                            local_path,
                            child,
                            input_index=public_index,
                        )
                    else:
                        child.set_size(len(buf))
                        local.makedirs(local._parent(local_path), exist_ok=True)
                        with open(local_path, "wb") as out:
                            out.write(buf)
                            child.relative_update(len(buf))
                del buf
                callback.relative_update()
            # The next batch should not coexist with this batch's payloads.
            del data

    def _upload_file_streamed(
        self,
        local_path,
        remote_path,
        mode,
        callback,
        *,
        source_size=None,
        create_parents=False,
        input_index=None,
        **open_kwargs,
    ):
        """Own upload endpoints and finish each chunk without unsafe replay."""
        _validate_write_mode(mode)
        on_progress = callback.relative_update if callback is not None else None
        internal = self._checked_strip_protocol(remote_path)
        with self._rollback_transaction_writers() as open_tracked, ExitStack() as stack:
            source = stack.enter_context(open(local_path, "rb"))
            callback.set_size(
                os.fstat(source.fileno()).st_size
                if source_size is None
                else source_size
            )
            if create_parents:
                open_kwargs["_create_parents"] = True
            native_mode = "xb" if mode == "create" else "wb"
            if input_index is None:
                remote = open_tracked(self.open, internal, native_mode, **open_kwargs)
            else:
                remote = open_tracked(
                    self._open_transfer_file,
                    internal,
                    native_mode,
                    input_index,
                    **open_kwargs,
                )
            stack.push(partial(_close_reader_on_exit, remote))
            while True:
                chunk = source.read(min(self.transfer_chunk_size, self.max_batch_bytes))
                if not chunk:
                    break
                _write_fully(remote, chunk, on_progress=on_progress)

    def put_file(
        self, lpath, rpath, callback=DEFAULT_CALLBACK, mode="overwrite", **kwargs
    ):
        """Upload one file with complete writes and atomic exclusive create."""
        _validate_write_mode(mode)
        internal = self._checked_strip_protocol(rpath)
        callback = Callback.as_callback(callback)
        if os.path.isdir(lpath):
            if self._intrans:
                raise NotImplementedError(
                    "transactional directory uploads are unsupported"
                )
            if mode == "create":
                if internal == "/" and not self._root:
                    self._client.ensure_ready()
                    raise FileExistsError(errno.EEXIST, "File exists", internal)
                self._ensure_dirs([internal], parents_only=True)
                self.mkdir(internal, create_parents=False)
            else:
                self.makedirs(internal, exist_ok=True)
            return
        self._upload_file_streamed(
            lpath, internal, mode, callback, create_parents=True, **kwargs
        )

    def put(
        self,
        lpath,
        rpath,
        recursive=False,
        callback=DEFAULT_CALLBACK,
        maxdepth=None,
        **kwargs,
    ):
        """Batch plain uploads; stream files with explicit open options."""
        mode = kwargs.pop("mode", "overwrite")
        if kwargs:
            for option in (
                "compression",
                "block_size",
                "cache_type",
                "cache_options",
                "write_buffering",
                "size",
            ):
                if kwargs.get(option, True) is None:
                    kwargs.pop(option)
        _validate_write_mode(mode)
        callback = Callback.as_callback(callback)
        from fsspec.implementations.local import (
            LocalFileSystem,
            make_path_posix,
            trailing_sep,
        )
        from fsspec.utils import other_paths

        lpath, rpath = _path_string(lpath), _path_string(rpath)
        if not isinstance(lpath, str) and not isinstance(rpath, str):
            lpath, rpath = list(lpath), list(rpath)
            if len(lpath) != len(rpath):
                raise ValueError("lpath and rpath must have equal length")
            rpaths = rpath
            lpaths = lpath
        else:
            source_is_str = isinstance(lpath, str)
            if source_is_str:
                lpath = make_path_posix(lpath)
            local = LocalFileSystem()
            # Older fsspec drops kwargs when scalar expansion delegates to
            # the list form. Enter that form directly to retain walk controls.
            lpaths = local.expand_path(
                [lpath] if source_is_str else lpath,
                recursive=recursive,
                maxdepth=maxdepth,
                **kwargs,
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
                self._checked_strip_protocol(rpath)
                if isinstance(rpath, str)
                else [self._checked_strip_protocol(p) for p in rpath]
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

        # Expansion has consumed these traversal controls; none configure OPEN.
        for option in ("assume_literal", "on_error", "refresh", "topdown"):
            kwargs.pop(option, None)
        callback.set_size(len(rpaths))
        pairs = []
        remote_dirs = []
        directory_indices = []
        for public_index, (local_path, remote_path) in enumerate(zip(lpaths, rpaths)):
            if os.path.isdir(local_path):
                remote_dirs.append(remote_path)
                directory_indices.append(public_index)
            else:
                pairs.append((local_path, remote_path, public_index))
        if remote_dirs:
            if self._intrans:
                raise NotImplementedError(
                    "transactional directory uploads are unsupported"
                )
            try:
                self._makedirs_batched(remote_dirs, exist_ok=True)
            except Exception as error:
                positions = {}
                for index, directory in zip(directory_indices, remote_dirs):
                    positions.setdefault(self._checked_strip_protocol(directory), index)
                _rebase_error(error, list(positions.values()), list(positions))
                raise
            for _ in remote_dirs:
                callback.relative_update()
        if not pairs:
            return
        sizes = [os.path.getsize(local_path) for local_path, _, _ in pairs]

        def upload_streamed(local_path, remote_path, size, public_index):
            with callback.branched(local_path, remote_path) as child:
                self._upload_file_streamed(
                    local_path,
                    remote_path,
                    mode,
                    child,
                    source_size=size,
                    input_index=public_index,
                    **kwargs,
                )
            callback.relative_update()

        def upload_batched(items):
            if not items:
                return
            self._write_batch(
                [
                    self._checked_strip_protocol(remote_path)
                    for _, remote_path, _, _ in items
                ],
                [data for _, _, data, _ in items],
                mode=mode,
                input_indices=[index for _, _, _, index in items],
            )
            for local_path, remote_path, data, _ in items:
                with callback.branched(local_path, remote_path) as child:
                    child.set_size(len(data))
                    child.relative_update(len(data))
                callback.relative_update()

        if kwargs:
            for (local_path, remote_path, public_index), size in zip(pairs, sizes):
                upload_streamed(local_path, remote_path, size, public_index)
            return
        for batch in _bounded_batches(sizes, self.batch_size, self.max_batch_bytes):
            if len(batch) == 1 and sizes[batch[0]] > self.max_batch_bytes:
                i = batch[0]
                local_path, remote_path, public_index = pairs[i]
                upload_streamed(local_path, remote_path, sizes[i], public_index)
                continue
            staged = []
            staged_bytes = 0
            for i in batch:
                local_path, remote_path, public_index = pairs[i]
                remaining = self.max_batch_bytes - staged_bytes
                with open(local_path, "rb") as fh:
                    data = fh.read(remaining + 1)
                if len(data) > remaining:
                    upload_batched(staged)
                    staged = []
                    staged_bytes = 0
                    if remaining < self.max_batch_bytes:
                        with open(local_path, "rb") as fh:
                            data = fh.read(self.max_batch_bytes + 1)
                    if len(data) > self.max_batch_bytes:
                        upload_streamed(local_path, remote_path, sizes[i], public_index)
                        continue
                staged.append((local_path, remote_path, data, public_index))
                staged_bytes += len(data)
            upload_batched(staged)

    # -- open / file objects ----------------------------------------------

    def _file_options(self, block_size, cache_type, cache_options, *, reading):
        if block_size is None:
            block_size = self.block_size
        elif block_size == "default":
            block_size = VfsiFile.DEFAULT_BLOCK_SIZE
        if isinstance(block_size, bool):
            raise ValueError("block_size must be a positive integer")
        try:
            block_size = operator.index(block_size)
        except TypeError:
            raise ValueError("block_size must be a positive integer") from None
        if block_size == 0:
            block_size = VfsiFile.DEFAULT_BLOCK_SIZE
        if block_size < 0:
            raise ValueError("block_size must be a positive integer")
        if cache_type is None:
            cache_type = self.cache_type
        if reading and cache_type not in caches:
            raise ValueError(f"unknown cache_type: {cache_type!r}")
        options = dict(
            (self.cache_options if cache_options is None else cache_options) or {}
        )
        if reading and cache_type not in (None, "none"):
            factory = _Nfs4MMapCache if cache_type == "mmap" else caches[cache_type]
            maxblocks = options.get("maxblocks")
            if (
                factory is BlockCache
                and maxblocks is not None
                and not isinstance(maxblocks, int)
            ):
                raise TypeError("blockcache maxblocks must be an integer or None")
            data = options.get("data")
            if factory is KnownPartsOfAFile and data is not None:
                if not isinstance(data, MutableMapping) or any(
                    not isinstance(bounds, tuple)
                    or len(bounds) != 2
                    or not all(isinstance(bound, int) for bound in bounds)
                    or not isinstance(value, bytes)
                    for bounds, value in data.items()
                ):
                    raise TypeError("parts cache data must map integer ranges to bytes")
            try:
                signature = inspect.signature(factory)
            except (TypeError, ValueError):
                # Some custom or extension cache factories expose no signature.
                pass
            else:
                signature.bind(block_size, None, 0, **options)
        return block_size, cache_type, options

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
        _defer_cache=False,
        _create_parents=False,
        **kwargs,
    ):
        internal = self._checked_strip_protocol(path)
        base_mode = _normalize_mode(mode)
        if not autocommit and "+" in base_mode:
            raise NotImplementedError("transactional update modes are unsupported")
        if not autocommit and any(c in mode for c in "wax"):
            # fsspec transactions: defer the write until commit()/discard().
            file = _DeferredWriteFile(
                self, internal, mode, create_parents=_create_parents
            )
            operation = _OPERATION_WRITERS.get()
            if operation is not None and operation[0] is self:
                operation[1].append(file)
            return file
        block_size, cache_type, cache_options = self._file_options(
            block_size, cache_type, cache_options, reading=base_mode == "r"
        )
        if (self.auto_mkdir or _create_parents) and any(c in mode for c in "wax"):
            parent = self._parent(path)
            if parent not in ("", "/") or (_create_parents and self._root):
                self._ensure_dirs([parent or "/"])
        fd = None
        identity = None
        try:
            if base_mode in ("r", "r+"):
                # A scalar open is the point at which Python promises a file
                # handle. Pin its native identity here; open_many() retains its
                # batched OPEN path for multi-file workloads.
                fd = self._client.open(
                    self._native_path(internal), "rb+" if base_mode == "r+" else "rb"
                )
                attrs = self._client.fstat(fd)
                if attrs["type"] == "directory":
                    raise IsADirectoryError(errno.EISDIR, "Is a directory", internal)
                identity = _handle_identity(attrs)
                if size is None:
                    size = _require_size(attrs, internal)
            file = VfsiFile(
                self,
                internal,
                mode,
                fd=fd,
                block_size=block_size,
                cache_type=cache_type,
                cache_options=cache_options,
                write_buffering=(
                    self.write_buffering
                    if write_buffering is None
                    else bool(write_buffering)
                ),
                size=size,
                defer_cache=_defer_cache,
                identity=identity,
            )
            setup = _SCALAR_CACHE_SETUP.get()
            if (
                setup is not None
                and setup[0] is self
                and setup[1] == internal
                and base_mode == "r"
            ):
                setup[2](file)
            return file
        except BaseException:
            if fd is not None:
                try:
                    self._client._close_live_token(fd)
                except BaseException:
                    self._client.defer_close_many([fd])
            raise

    def _dependent_group_readers(self, paths, modes, writers, missing):
        """Identify missing reader targets that an earlier OPEN may create."""
        latest = max(missing)
        exclusive = [
            index for index in writers if index < latest and modes[index][0] == "x"
        ]
        unchecked_creator = None
        if exclusive:
            # Without LSTAT, the native binding follows the final component.
            # A dangling link can then look absent and still block exclusive OPEN.
            lstat_flag = getattr(self._native_module, "CAP_LSTAT", 0)
            capabilities = getattr(self._client, "capabilities", None)
            if not (lstat_flag and capabilities and capabilities() & lstat_flag):
                # A sole writer can rely on exclusive OPEN itself. A dangling
                # link collision cannot truncate or create an unrelated name.
                if len(writers) == 1:
                    unchecked_creator = exclusive[0]
                exclusive = []
        absent_exclusive = set()
        for batch in _bounded_batches(
            [len(os.fsencode(self._native_path(paths[index]))) for index in exclusive],
            self.batch_size,
            self.max_batch_bytes,
        ):
            native = [self._native_path(paths[exclusive[index]]) for index in batch]
            try:
                _, errors = self._client.lstat_many(native)
            except (OSError, NotImplementedError):
                continue
            absent_exclusive.update(
                exclusive[batch[slot]]
                for slot, code in errors.items()
                if _errno(code) == errno.ENOENT
            )
        creators = {}
        for index in writers:
            if index >= latest:
                break
            if modes[index][0] != "x" or index in absent_exclusive:
                creators.setdefault(paths[index], index)
        dependent = {
            index for index in missing if creators.get(paths[index], index) < index
        }
        if unchecked_creator is not None:
            dependent.update(
                index
                for index in missing
                if unchecked_creator < index
                and paths[index] == paths[unchecked_creator]
            )
        unresolved = sorted(missing - dependent)
        if creators and unresolved:
            aliases, _ = self._canonical_namespace_paths(
                [*creators, *(paths[index] for index in unresolved)],
                follow_final=True,
            )
            first_creator = {}
            for path, index in creators.items():
                if path in aliases:
                    first_creator.setdefault(aliases[path], index)
            dependent.update(
                index
                for index in unresolved
                if paths[index] in aliases
                and first_creator.get(aliases[paths[index]], index) < index
            )
        return dependent

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
        """Open a group of ``OpenFile`` objects using native vectors.

        Eager reads and mmap backing files are prepared before opening writers.
        Missing reader paths can open with an earlier writer that may create
        them. With multiple writers, an exclusive creator requires support for
        checking the final entry without following symlinks. A sole exclusive
        writer can create a file for a later reader using the same spelling.
        Use the same spelling for new SMB names
        because the share's case-equivalence policy may vary by directory.
        An empty file needs no backing map.
        Supplied sizes are nonnegative integer file sizes. An explicit mmap
        location can back one buffered reader.
        """
        paths = [self._checked_strip_protocol(f.path) for f in open_files]
        modes = [_normalize_mode(f.mode) + "b" for f in open_files]
        effective_block_size, effective_cache_type, effective_cache_options = (
            self._file_options(
                block_size, cache_type, cache_options, reading="rb" in modes
            )
        )
        if (
            effective_cache_type == "mmap"
            and effective_cache_options.get("location") is not None
            and modes.count("rb") > 1
        ):
            raise ValueError("an mmap cache location can back only one grouped reader")
        if any(mode[0] in "wax" for mode in modes):
            for open_file in open_files:
                if "b" not in open_file.mode:
                    _validate_text_options(
                        {
                            "encoding": open_file.encoding,
                            "errors": open_file.errors,
                            "newline": open_file.newline,
                        }
                    )
        effective_write_buffering = (
            self.write_buffering if write_buffering is None else bool(write_buffering)
        )
        if sizes is None:
            discovered_sizes = [None] * len(paths)
        else:
            discovered_sizes = list(sizes)
            if len(discovered_sizes) != len(paths):
                raise ValueError("sizes must have one entry per open file")
            for index, size in enumerate(discovered_sizes):
                if size is not None:
                    size = operator.index(size)
                    if not 0 <= size <= (1 << 64) - 1:
                        raise ValueError(
                            f"sizes[{index}] must fit an unsigned 64-bit file size"
                        )
                    discovered_sizes[index] = size
        eager_readers = (
            "rb" in modes
            and caches[effective_cache_type] is AllBytes
            and effective_cache_options.get("data") is None
        )
        mmap_readers = effective_cache_type == "mmap"
        if eager_readers:
            for path, mode, size in zip(paths, modes, discovered_sizes):
                if mode == "rb" and size is not None:
                    self._check_read_allocation(size, path)
        if self._intrans and any(_normalize_mode(mode) != "r" for mode in modes):
            if any("+" in _normalize_mode(mode) for mode in modes):
                raise NotImplementedError("transactional update modes are unsupported")
            # OpenFile already handles text/compression finalization and scalar
            # transaction enrollment. Reuse it instead of a second coordinator.
            with self._rollback_transaction_writers() as open_tracked:
                return open_tracked(_enter_individually, open_files)
        mutating_indices = [
            index for index, mode in enumerate(modes) if mode[0] in "wax"
        ]
        mutating_paths = [paths[index] for index in mutating_indices]
        fds = []
        opened_files = {}

        def group_fstat(indices, descriptors):
            try:
                attrs = self._client.fstat_many(descriptors)
            except Exception as error:
                _rebase_error(error, indices, [paths[index] for index in indices])
                raise
            if len(attrs) != len(indices):
                raise OSError(errno.EIO, "native fstat returned the wrong result count")
            return attrs

        def open_group(indices, changed_paths):
            selected_paths = [paths[index] for index in indices]
            try:
                with self._cache_mutation(changed_paths, namespace=True):
                    opened = self._client.open_many(
                        [self._native_path(path) for path in selected_paths],
                        [modes[index] for index in indices],
                    )
                    fds.extend(opened)
            except Exception as error:
                _rebase_error(error, indices, selected_paths)
                raise
            if len(opened) != len(indices):
                raise OSError(
                    errno.EIO, "native open returned the wrong descriptor count"
                )

            descriptors = dict(zip(indices, opened))
            metadata_indices = [
                index for index in indices if modes[index] in ("rb", "ab", "a+b")
            ]
            identities = {}
            if metadata_indices:
                attrs = group_fstat(
                    metadata_indices, [descriptors[index] for index in metadata_indices]
                )
                for index, attr in zip(metadata_indices, attrs):
                    if attr["type"] == "directory":
                        raise IsADirectoryError(
                            errno.EISDIR, "Is a directory", paths[index]
                        )
                    identities[index] = _handle_identity(attr)
                    # Append positions come from the opened handle because
                    # caller-supplied size hints may be stale.
                    if discovered_sizes[index] is None or modes[index] in ("ab", "a+b"):
                        discovered_sizes[index] = _require_size(
                            attr, paths[index], index=index
                        )
            for index, fd in descriptors.items():
                opened_files[index] = VfsiFile(
                    self,
                    paths[index],
                    modes[index],
                    fd=fd,
                    block_size=effective_block_size,
                    cache_type=effective_cache_type,
                    cache_options=effective_cache_options,
                    write_buffering=effective_write_buffering,
                    size=discovered_sizes[index],
                    defer_cache=True,
                    identity=identities.get(index),
                )

        try:
            pending = range(len(paths))
            mmap_discovered = []
            if (eager_readers or mmap_readers) and mutating_paths:
                readers = []
                possible_creations = []
                emptied_paths = set()
                earlier_creator = False
                for index, (path, mode) in enumerate(zip(paths, modes)):
                    if mode[0] in "wax":
                        earlier_creator = True
                        if mode[0] == "w":
                            emptied_paths.add(path)
                    elif mode == "rb":
                        size = discovered_sizes[index]
                        empty_mmap = mmap_readers and size in (None, 0)
                        if empty_mmap and path in emptied_paths:
                            continue
                        if earlier_creator and (eager_readers or empty_mmap):
                            possible_creations.append(index)
                        readers.append(index)

                if possible_creations:
                    lookup = [paths[index] for index in possible_creations]
                    try:
                        _, errors = self._client.stat_many(
                            [self._native_path(path) for path in lookup]
                        )
                    except Exception as error:
                        _rebase_error(error, possible_creations, lookup)
                        raise
                    # An earlier append or a writer reached through a dangling
                    # link can create the empty file in the original batch.
                    missing = {
                        index
                        for relative, index in enumerate(possible_creations)
                        if relative in errors
                        and _errno(errors[relative]) == errno.ENOENT
                    }
                    if missing:
                        dependent = self._dependent_group_readers(
                            paths, modes, mutating_indices, missing
                        )
                        readers = [index for index in readers if index not in dependent]

                if readers:
                    if mmap_readers:
                        mmap_discovered = [
                            index
                            for index in readers
                            if discovered_sizes[index] is None
                        ]
                    open_group(readers, [])
                    try:
                        _BufferGroup(self, [opened_files[index] for index in readers])
                    except Exception as error:
                        _rebase_error(
                            error, readers, [paths[index] for index in readers]
                        )
                        raise
                    pending = [index for index in pending if index not in opened_files]

            if self.auto_mkdir and mutating_paths:
                try:
                    self._ensure_file_parents(mutating_paths)
                except Exception as error:
                    _rebase_error(error, mutating_indices, mutating_paths)
                    raise
            open_group(pending, mutating_paths)
            if mmap_discovered and any(
                modes[index][0] == "w" for index in mutating_indices
            ):
                # A differently named writer may have truncated the pinned
                # inode. Shorten its logical cache without remapping or
                # losing the earlier backing-file validation.
                attrs = group_fstat(
                    mmap_discovered,
                    [opened_files[index]._fd for index in mmap_discovered],
                )
                for index, attr in zip(mmap_discovered, attrs):
                    file = opened_files[index]
                    size = min(
                        file.size, _require_size(attr, paths[index], index=index)
                    )
                    file.size = size
                    file.cache.size = size
            files = [opened_files[index] for index in range(len(paths))]
            _BufferGroup(self, files)
        except BaseException as setup_error:
            # Constructors may release an original token. Cache setup may
            # also replace one while recovering, before a later stage fails.
            current = [file._fd for file in opened_files.values()]
            cleanup_error = self._client._close_setup_handles(
                fds + current, self.batch_size
            )
            local_error = _finish_group_files(opened_files.values())
            cleanup_error = cleanup_error or local_error
            if cleanup_error is not None:
                raise setup_error from cleanup_error
            raise
        if any(f.compression is not None or "b" not in f.mode for f in open_files):
            wrapped = _WrappedFiles(files, open_files)
            try:
                return wrapped.wrap()
            except BaseException as error:
                try:
                    self.commit_many(wrapped)
                except BaseException as cleanup_error:
                    raise error from cleanup_error
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
        if isinstance(open_files, _WrappedFiles):
            return open_files.group.finish()
        error = None
        finalized_groups = set()
        for file in open_files:
            group = getattr(file, "_buffer_group", None)
            try:
                if group is None:
                    file.close()
                elif id(group) not in finalized_groups:
                    finalized_groups.add(id(group))
                    # Group finalization closes every owned handle, so flush
                    # every sibling and keep other groups on their own owner.
                    group.finish()
            except BaseException as exc:
                error = error or exc
        if error is not None:
            raise error

    # -- mutation ----------------------------------------------------------

    def mkdir(self, path, create_parents=True, **kwargs):
        internal = self._checked_strip_protocol(path)
        mode = kwargs.get("mode")
        mode = (self._default_dir_mode if mode is None else mode) & 0o7777
        if create_parents:
            if self.exists(internal):
                raise FileExistsError(errno.EEXIST, "File exists", internal)
            if mode == self._default_dir_mode:
                self._ensure_dirs([internal], verify_existing=True)
                return
            self._ensure_dirs([posixpath.dirname(internal) or "/"])
        with self._cache_mutation([internal], namespace=True):
            self._client.mkdir(self._native_path(internal), mode)

    def makedirs(self, path, exist_ok=False):
        self._makedirs_batched([path], exist_ok)

    def rmdir(self, path):
        internal = self._checked_strip_protocol(path)
        # Use the same capability-aware metadata path as rm and makedirs:
        # NFS/local retain no-follow semantics; SMB has no LSTAT capability.
        stats, errors = self._client.lstat_many([self._native_path(internal)])
        attrs = stats[0]
        if attrs is None:
            raise _oserror(errors.get(0, errno.EIO), internal)
        if attrs["type"] != "directory":
            raise NotADirectoryError(errno.ENOTDIR, "Not a directory", internal)
        with self._cache_mutation([internal], namespace=True):
            self._client.remove_many([self._native_path(internal)])

    def rm_file(self, path):
        """Remove one non-directory path using the native vector primitive."""
        self.rm(path, recursive=False)

    def rm(self, path, recursive=False, maxdepth=None):
        if maxdepth is not None and maxdepth < 1:
            raise ValueError("maxdepth must be at least 1")
        if isinstance(path, (str, os.PathLike)):
            paths = [self._checked_strip_protocol(path)]
        else:
            paths = [self._checked_strip_protocol(p) for p in path]
        with self._cache_mutation(paths, namespace=True):
            if recursive:
                self._rm_recursive(paths, maxdepth=maxdepth)
            else:
                # Like LocalFileSystem, non-recursive rm must not remove
                # directories (lstat so symlinks-to-directories are removed as
                # links, matching os.remove).
                stats, errors = self._client.lstat_many(
                    [self._native_path(p) for p in paths]
                )
                first_error = None
                for i, s in enumerate(stats):
                    if i in errors or s is None:
                        if first_error is None:
                            first_error = (i, errors.get(i, errno.EIO))
                    elif s["type"] == "directory":
                        raise ValueError("Cannot delete directory, set recursive=True")
                if first_error is None:
                    self._client.remove_many([self._native_path(p) for p in paths])
                    return
                # A failed classification cannot authorize removal of that
                # input, but validated siblings retain independent execution.
                active = [
                    i for i, s in enumerate(stats) if s is not None and i not in errors
                ]
                try:
                    if active:
                        self._client.remove_many(
                            [self._native_path(paths[i]) for i in active]
                        )
                except Exception as error:
                    position = getattr(error, "index", None)
                    if (
                        not isinstance(position, int)
                        or isinstance(position, bool)
                        or not 0 <= position < len(active)
                    ):
                        raise
                    index = active[position]
                    # A lost REMOVE reply leaves mutation state uncertain.
                    if _is_uncertain_mutation(error) or index < first_error[0]:
                        _oserror(error, paths[index], index=index)
                        raise
                index, error = first_error
                raise _oserror(error, paths[index], index=index)

    def _rm_recursive(self, paths, maxdepth=None):
        """Recursively remove ``paths`` with a batched walk + removev."""
        native_paths = [self._native_path(p) for p in paths]
        lstats, lerrs = self._client.lstat_many(native_paths)
        if lerrs:
            i = min(lerrs)
            raise _oserror(lerrs[i], paths[i])

        remove_files = []
        remove_dirs = []
        remove_empty_dirs = []
        for i, p in enumerate(paths):
            if lstats[i] is None:
                continue
            if lstats[i]["type"] != "directory":
                remove_files.append(native_paths[i])
                continue
            if maxdepth is not None:
                tree = self._client.walk(
                    native_paths[i],
                    sort=True,
                    max_depth=min(self.walk_max_depth, maxdepth - 1),
                )
                root_depth = _depth(paths[i])
                if maxdepth > self.walk_max_depth + 1 and any(
                    entry["type"] == "directory"
                    and _depth(self._internalize(entry["name"])) - root_depth
                    > self.walk_max_depth
                    for _, entries in tree
                    for entry in entries
                ):
                    raise _oserror(errno.EFBIG, paths[i])
                remove_empty_dirs.append(native_paths[i])
                for _, entries in tree:
                    for entry in entries:
                        internal = self._internalize(entry["name"])
                        relative_depth = _depth(internal) - root_depth
                        if entry["type"] == "directory":
                            if relative_depth <= maxdepth:
                                remove_empty_dirs.append(entry["name"])
                        elif relative_depth <= maxdepth:
                            remove_files.append(entry["name"])
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
        remove_empty_dirs = list(dict.fromkeys(remove_empty_dirs))
        remove_empty_dirs.sort(key=lambda p: p.count("/"), reverse=True)
        for directory in remove_empty_dirs:
            tree = self._client.walk(directory, sort=True, max_depth=0)
            if tree and not tree[0][1]:
                self._client.remove_many([directory])
        remove_dirs = list(dict.fromkeys(remove_dirs))
        remove_dirs.sort(key=lambda p: p.count("/"), reverse=True)
        # A vector may execute in any order. Keep parent/child dependencies
        # between calls while batching independent directories at each depth.
        for _, directories in groupby(remove_dirs, key=lambda p: p.count("/")):
            self._client.remove_many(list(directories))

    def mv(self, path1, path2, recursive=False, maxdepth=None, **kwargs):
        if isinstance(path1, list) or isinstance(path2, list):
            if not (
                isinstance(path1, list)
                and isinstance(path2, list)
                and len(path1) == len(path2)
            ):
                raise ValueError("path1 and path2 must both be lists of equal length")
            pairs = [
                (self._checked_strip_protocol(a), self._checked_strip_protocol(b))
                for a, b in zip(path1, path2)
            ]
        else:
            src = self._checked_strip_protocol(path1)
            dst = self._checked_strip_protocol(path2)
            # Same-path moves still reach rename so a missing source cannot
            # report success. Do not reinterpret that directory as its child.
            if src != dst and self.isdir(dst):
                # Like shutil.move / base copy+rm: moving onto an existing
                # directory moves the source inside it.
                dst = posixpath.join(dst, posixpath.basename(src))
                entries, errors = self._client.lstat_many([self._native_path(dst)])
                if entries[0] is not None:
                    raise shutil.Error(f"Destination path {dst!r} already exists")
                if 0 in errors and _errno(errors[0]) != errno.ENOENT:
                    raise _oserror(errors[0], dst)
            pairs = [(src, dst)]
        with self._cache_mutation(
            [path for pair in pairs for path in pair], namespace=True
        ):
            self._client.rename_many(
                [(self._native_path(a), self._native_path(b)) for a, b in pairs]
            )

    def _copy_recursive(
        self, src, dst, symlinks=False, callback=DEFAULT_CALLBACK, on_error="raise"
    ):
        """Copy a directory tree with batched walk/mkdir/copy calls."""
        native_src = self._native_path(src)
        src = src.rstrip("/") or "/"
        dst = dst.rstrip("/") or "/"
        tree = self._client.walk(native_src)

        dest_dirs = {dst}
        pairs = []
        symlink_pairs = []
        followed_pairs = []
        unclassified_pairs = []
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
                elif e["type"] == "symlink":
                    followed_pairs.append((child_int, dest))
                elif e["type"] == "file":
                    pairs.append((child_int, dest))
                else:
                    unclassified_pairs.append((child_int, dest))

        if followed_pairs:
            stats, _ = self._client.stat_many(
                [self._native_path(src) for src, _ in followed_pairs]
            )
            for pair, info in zip(followed_pairs, stats):
                if info is not None and info["type"] not in ("file", "directory"):
                    unclassified_pairs.append(pair)
                else:
                    # Retain native COPY's path errors and keep ordinary
                    # followed file links in the same batch as other files.
                    pairs.append(pair)

        callback.set_size(len(pairs) + len(symlink_pairs) + len(unclassified_pairs))
        self._ensure_dirs(list(dest_dirs))
        self._copy_pairs(
            pairs,
            on_error,
            callback=callback,
            callback_is_parent=True,
            set_callback_size=False,
        )
        self._copy_symlink_pairs(symlink_pairs, on_error, callback)
        self._copy_classified_pairs(
            unclassified_pairs,
            on_error,
            callback,
            symlinks=symlinks,
            set_callback_size=False,
        )

    def cp_file(self, path1, path2, callback=DEFAULT_CALLBACK, **kwargs):
        """Copy a single file (or create a directory) between two paths."""
        callback = Callback.as_callback(callback)
        src = self._checked_strip_protocol(path1)
        dst = self._checked_strip_protocol(path2)
        info = self.info(src)
        if info["type"] == "directory":
            dst_stats, dst_errors = self._client.stat_many([self._native_path(dst)])
            if 0 in dst_errors and _errno(dst_errors[0]) != errno.ENOENT:
                raise _oserror(dst_errors[0], dst) from None
            if dst_stats[0] is not None and dst_stats[0]["type"] != "directory":
                raise FileExistsError(errno.EEXIST, "File exists", dst)
            self._ensure_dirs([dst])
            _complete_callback(callback, 0)
            return
        if info["type"] != "file":
            raise FileNotFoundError(src)
        parent = self._parent(dst)
        if self.auto_mkdir and parent not in ("", "/"):
            self._ensure_dirs([parent])
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
        preserve_links = kwargs.get("symlinks", False)
        # Convert scalar paths before checking their shape, retaining separators
        # used by fsspec to distinguish directory contents from the directory.
        path1 = _path_string(path1)
        path2 = _path_string(path2)
        if on_error is None:
            on_error = "ignore" if recursive else "raise"
        if not isinstance(path1, str) and not isinstance(path2, str):
            # Paired inputs must stay paired: expand_path sorts and deduplicates
            # source collections, which would change their destination mapping.
            path1, path2 = list(path1), list(path2)
            if len(path1) != len(path2):
                raise ValueError("path1 and path2 must be sequences of equal length")
            pairs = [
                (self._checked_strip_protocol(a), self._checked_strip_protocol(b))
                for a, b in zip(path1, path2)
            ]
            if not pairs:
                callback.set_size(0)
                return
            if preserve_links:
                self._copy_classified_pairs(pairs, on_error, callback, symlinks=True)
            else:
                # Check special sources together before native COPY can open a
                # FIFO. Native COPY retains its existing per-item path errors.
                stats, _ = self._client.stat_many(
                    [self._native_path(src) for src, _ in pairs]
                )
                callback.set_size(len(pairs))
                pending = []
                positions = []
                first_special = None
                for index, (pair, info) in enumerate(zip(pairs, stats)):
                    if info is not None and info["type"] not in ("file", "directory"):
                        callback.relative_update()
                        if first_special is None:
                            first_special = pair[0]
                    else:
                        pending.append(pair)
                        positions.append(index)
                self._copy_pairs(
                    pending,
                    on_error,
                    callback=callback,
                    callback_is_parent=True,
                    set_callback_size=False,
                    positions=positions,
                )
                if first_special is not None and on_error == "raise":
                    raise FileNotFoundError(first_special)
            return
        # Batched tree copy for the plain "directory -> new name" case.
        if (
            isinstance(path1, str)
            and isinstance(path2, str)
            and not has_magic(path1)
            and recursive
            and maxdepth is None
            and not path1.endswith("/")
            and not (isinstance(path2, str) and path2.endswith("/"))
        ):
            src = self._checked_strip_protocol(path1)
            dst = self._checked_strip_protocol(path2)
            metadata = (
                self._client.lstat_many if preserve_links else self._client.stat_many
            )
            stats, errors = metadata([self._native_path(src), self._native_path(dst)])
            if (
                preserve_links
                and stats[1] is not None
                and stats[1]["type"] == "symlink"
            ):
                # Source links must remain links, while a destination link to a
                # directory still has the ordinary copy-into-directory meaning.
                followed, follow_errors = self._client.stat_many(
                    [self._native_path(dst)]
                )
                stats[1] = followed[0]
                errors.pop(1, None)
                if 0 in follow_errors:
                    errors[1] = follow_errors[0]
            src_is_dir = stats[0] is not None and stats[0]["type"] == "directory"
            dst_is_dir = stats[1] is not None and stats[1]["type"] == "directory"
            if errors:
                i = min(errors)
                if _errno(errors[i]) != errno.ENOENT:
                    raise _oserror(errors[i], (src, dst)[i])
            if src_is_dir and stats[1] is not None and not dst_is_dir:
                raise FileExistsError(errno.EEXIST, "File exists", dst)
            if src_is_dir and not dst_is_dir:
                self._copy_recursive(
                    src,
                    dst,
                    preserve_links,
                    callback=callback,
                    on_error=on_error,
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
            if preserve_links:
                candidates = [p for p in paths1 if not trailing_sep(p)]
                stats, _ = self._client.lstat_many(
                    [
                        self._native_path(self._checked_strip_protocol(p))
                        for p in candidates
                    ]
                )
                paths1 = [
                    p
                    for p, info in zip(candidates, stats)
                    if info is None or info["type"] != "directory"
                ]
            else:
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
            (self._checked_strip_protocol(a), self._checked_strip_protocol(b))
            for a, b in zip(paths1, paths2)
        ]
        self._copy_classified_pairs(pairs, on_error, callback, symlinks=preserve_links)

    def _copy_classified_pairs(
        self, pairs, on_error, callback, symlinks=False, *, set_callback_size=True
    ):
        if set_callback_size:
            callback.set_size(len(pairs))
        if not pairs:
            return
        metadata = self._client.lstat_many if symlinks else self._client.stat_many
        stats, errors = metadata([self._native_path(src) for src, _ in pairs])
        file_pairs = []
        file_indices = []
        dir_pairs = []
        dir_indices = []
        symlink_pairs = []
        symlink_indices = []
        first_error = None
        for i, ((src, dst), stat) in enumerate(zip(pairs, stats)):
            if i in errors:
                callback.relative_update()
                if on_error == "raise" and first_error is None:
                    first_error = _oserror(errors[i], src)
            elif stat is not None and stat["type"] == "directory":
                dir_pairs.append((src, dst))
                dir_indices.append(i)
            elif stat is not None and stat["type"] == "file":
                file_pairs.append((src, dst))
                file_indices.append(i)
            elif symlinks and stat is not None and stat["type"] == "symlink":
                symlink_pairs.append((src, dst))
                symlink_indices.append(i)
            else:
                callback.relative_update()
                if on_error == "raise" and first_error is None:
                    first_error = FileNotFoundError(src)
        if dir_pairs:
            try:
                self._makedirs_batched([dst for _, dst in dir_pairs], exist_ok=True)
            except Exception as error:
                positions = {}
                for index, (_, destination) in zip(dir_indices, dir_pairs):
                    positions.setdefault(destination, index)
                _rebase_error(error, list(positions.values()), list(positions))
                raise
            for src, dst in dir_pairs:
                _complete_child(callback, src, dst, 0)
        self._copy_pairs(
            file_pairs,
            on_error,
            callback=callback,
            callback_is_parent=True,
            set_callback_size=False,
            positions=file_indices,
        )
        self._copy_symlink_pairs(
            symlink_pairs, on_error, callback, positions=symlink_indices
        )
        if first_error is not None:
            raise first_error

    def _copy_symlink_pairs(self, pairs, on_error, callback, *, positions=None):
        """Copy stored targets without following links or adding the root twice."""
        if not pairs:
            return
        # Like shutil.copyfile, reject identical objects even when preserving
        # the source link. Batch the followed identity probes across all pairs.
        try:
            stats, _ = self._client.stat_many(
                [self._native_path(path) for pair in pairs for path in pair]
            )
        except Exception as error:
            _rebase_error(
                error,
                [
                    index if positions is None else positions[index]
                    for index in range(len(pairs))
                    for _ in range(2)
                ],
                [path for pair in pairs for path in pair],
            )
            raise
        for index, (src, dst) in enumerate(pairs):
            try:
                source, destination = stats[2 * index : 2 * index + 2]
                if source is not None and destination is not None:
                    identity = _handle_identity(source)
                    if src == dst or (
                        identity is not None
                        and identity == _handle_identity(destination)
                    ):
                        raise shutil.SameFileError(
                            f"{src!r} and {dst!r} are the same file"
                        )
                target = self.readlink(src)
                parent = posixpath.dirname(dst)
                if self.auto_mkdir and parent not in ("", "/"):
                    self._ensure_dirs([parent])
                with self._cache_mutation([dst], namespace=True):
                    self._client.symlink(target, self._native_path(dst))
            except (OSError, NotImplementedError, shutil.SameFileError) as error:
                _oserror(
                    error, dst, index=index if positions is None else positions[index]
                )
                # Never suppress or replay an ambiguous link creation.
                if _is_uncertain_mutation(error):
                    raise
                callback.relative_update()
                if on_error == "raise":
                    raise
            else:
                _complete_child(callback, src, dst, 0)

    cp = copy

    def _copy_pairs(
        self,
        pairs,
        on_error,
        callback=None,
        callback_is_parent=False,
        set_callback_size=True,
        positions=None,
    ):
        if callback is not None and set_callback_size and callback_is_parent:
            callback.set_size(len(pairs))
        if not pairs:
            return
        with self._cache_mutation([dst for _, dst in pairs], namespace=True):
            native_pairs = [
                (self._native_path(a), self._native_path(b)) for a, b in pairs
            ]

            def copy_batch(batch, indices):
                try:
                    copied, errors = self._client.copy_many(batch)
                except Exception as error:
                    _rebase_error(
                        error,
                        (
                            indices
                            if positions is None
                            else [positions[i] for i in indices]
                        ),
                        [pairs[i][0] for i in indices],
                    )
                    raise
                if errors:
                    uncertain = min(
                        (
                            i
                            for i, error in errors.items()
                            if _is_uncertain_mutation(error)
                        ),
                        default=None,
                    )
                    if uncertain is not None:
                        index = indices[uncertain]
                        raise _oserror(
                            errors[uncertain],
                            pairs[index][0],
                            index=index if positions is None else positions[index],
                        )
                return copied, errors

            copied, errors = copy_batch(native_pairs, range(len(pairs)))
            missing = []
            if self.auto_mkdir:
                missing = sorted(
                    index
                    for index, code in errors.items()
                    if _errno(code) == errno.ENOENT
                )
                if missing:
                    # ENOENT may have come from the source rather than the
                    # destination parent. Confirm only the failed sources.
                    try:
                        stats, _ = self._client.stat_many(
                            [native_pairs[index][0] for index in missing]
                        )
                    except Exception as error:
                        _rebase_error(
                            error,
                            (
                                missing
                                if positions is None
                                else [positions[i] for i in missing]
                            ),
                            [pairs[index][0] for index in missing],
                        )
                        raise
                    missing = [
                        index
                        for index, info in zip(missing, stats)
                        if info is not None and info["type"] in ("file", "directory")
                    ]
            if missing:
                # Retry only missing destination parents. Leave successes and
                # unrelated failures from the original batch alone.
                parents = {
                    posixpath.dirname(native_pairs[index][1]) for index in missing
                }
                internal_parents = [
                    self._internalize(parent)
                    for parent in sorted(parents)
                    if parent not in ("", "/")
                ]
                parent_errors = {}

                def rebase_parent_error(error, parents):
                    destinations = [pairs[index][1] for index in missing]
                    self._rebase_directory_error(error, parents, destinations)
                    _rebase_error(
                        error,
                        (
                            missing
                            if positions is None
                            else [positions[index] for index in missing]
                        ),
                        destinations,
                    )

                try:
                    self._ensure_dirs(internal_parents)
                except OSError as error:
                    if _is_uncertain_mutation(error) or error.errno is None:
                        rebase_parent_error(error, internal_parents)
                        raise
                    # Only a semantic failure needs parent-by-parent recovery.
                    # A bad parent must not block independent successful copies
                    # or bypass the original per-item error policy.
                    for parent in internal_parents:
                        try:
                            self._ensure_dirs([parent])
                        except OSError as failure:
                            if _is_uncertain_mutation(failure) or failure.errno is None:
                                rebase_parent_error(failure, [parent])
                                raise
                            parent_errors[parent] = failure
                ready = []
                for index in missing:
                    parent = self._internalize(
                        posixpath.dirname(native_pairs[index][1])
                    )
                    if parent in parent_errors:
                        errors[index] = parent_errors[parent]
                    else:
                        ready.append(index)
                if ready:
                    retried, retry_errors = copy_batch(
                        [native_pairs[index] for index in ready], ready
                    )
                    for position, index in enumerate(ready):
                        copied[index] = retried[position]
                        errors.pop(index, None)
                    errors.update(
                        {
                            ready[position]: code
                            for position, code in retry_errors.items()
                        }
                    )
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
                index = i if positions is None else positions[i]
                if _errno(errors[i]) == _ERR_SAME_FILE:
                    src, dst = pairs[i]
                    exc = shutil.SameFileError(f"{src!r} and {dst!r} are the same file")
                    exc.index = index
                else:
                    exc = _oserror(errors[i], pairs[i][0], index=index)
                if on_error == "raise":
                    raise exc

    def touch(self, path, truncate=True, **kwargs):
        internal = self._checked_strip_protocol(path)
        if truncate:
            self._write_batch([internal], [b""])
            return
        with self._cache_mutation([internal], namespace=True):
            native = self._native_path(internal)
            try:
                self._client.touch(native)
            except FileNotFoundError:
                if self._intrans:
                    # Existing staged content already creates/touches this name.
                    # Otherwise publish exclusively to preserve concurrent creators.
                    if not any(
                        file.path == internal for file in self.transaction.files
                    ):
                        self._write_batch([internal], [b""], mode="create")
                else:
                    if self.auto_mkdir:
                        self._ensure_dirs([posixpath.dirname(internal)])
                    # The name may now exist. Empty nontruncating writes also
                    # preserve the target of a symlink installed by another writer.
                    self._client.write_many([native], [b""], truncate=False)
                    self._client.touch(native)

    def symlink(self, target, path, **kwargs):
        target = _path_string(target)
        with self._cache_mutation([path], namespace=True):
            link = self._native_path(self._checked_strip_protocol(path))
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
                # Map absolute targets through the configured path prefix.
                # Relative targets are stored as-is and may resolve outside that
                # prefix; the server export is the actual security boundary.
                target = self._native_path(self._checked_strip_protocol(target))
            self._client.symlink(target, link)

    def readlink(self, path):
        return self._client.readlink(
            self._native_path(self._checked_strip_protocol(path))
        )

    def hardlink(self, src, dst):
        # LINK updates the source inode, including the symlink inode when
        # the source is a symlink.
        with self._cache_mutation([src, dst], namespace=True, follow_final=False):
            self._client.hardlink(
                self._native_path(self._checked_strip_protocol(src)),
                self._native_path(self._checked_strip_protocol(dst)),
            )

    # -- traversal ---------------------------------------------------------

    def walk(self, path, maxdepth=None, topdown=True, on_error="omit", **kwargs):
        if maxdepth is not None and maxdepth < 1:
            raise ValueError("maxdepth must be at least 1")
        detail = kwargs.pop("detail", False)
        refresh = kwargs.pop("refresh", False)
        internal = self._checked_strip_protocol(path)
        tree = (
            None if refresh or topdown else self._cached_walk_tree(internal, maxdepth)
        )
        if tree is None:
            if not topdown or refresh:
                self.invalidate_cache(internal)
            if topdown:
                # Fetch each directory only after its parent has been yielded.
                # This lets callers prune the mutable dirs collection before
                # an inaccessible or expensive child is listed.
                root_depth = _depth(internal)
                requested_depth = None if maxdepth is None else maxdepth - 1
                descend_limit = (
                    self.walk_max_depth
                    if requested_depth is None
                    else min(self.walk_max_depth, requested_depth)
                )
                hard_limit = (
                    requested_depth is None or requested_depth > self.walk_max_depth
                )
                pending = [internal]
                budget = _WalkBudget(self, internal)
                while pending:
                    directory = pending.pop()
                    budget.sync()
                    prepaid = directory in budget.prefetched
                    try:
                        if prepaid:
                            infos = self.ls(directory, detail=True, _walk_budget=budget)
                        elif (
                            directory == internal
                            and self._cached_listing(directory) is None
                        ):
                            try:
                                root_tree = self._native_walk_tree(
                                    internal,
                                    self._dircache_epoch_snapshot(),
                                    maxdepth=1,
                                )
                            except NotADirectoryError:
                                infos = self.ls(
                                    directory, detail=True, _walk_budget=budget
                                )
                            except (FileNotFoundError, OSError) as error:
                                if getattr(error, "errno", None) == errno.EFBIG:
                                    raise
                                infos = self.ls(
                                    directory, detail=True, _walk_budget=budget
                                )
                            else:
                                infos = (
                                    root_tree[0][1]
                                    if root_tree
                                    else self.ls(
                                        directory, detail=True, _walk_budget=budget
                                    )
                                )
                        else:
                            infos = self.ls(directory, detail=True, _walk_budget=budget)
                    except (FileNotFoundError, OSError) as error:
                        if getattr(error, "errno", None) == errno.EFBIG:
                            raise
                        if on_error == "raise":
                            raise
                        if callable(on_error):
                            on_error(error)
                        continue
                    if not prepaid:
                        budget.account(infos)
                    if (
                        directory == internal
                        and len(infos) == 1
                        and infos[0]["type"] != "directory"
                        and self._checked_strip_protocol(infos[0]["name"]) == internal
                    ):
                        file_info = dict(infos[0])
                        files = {"": file_info} if detail else [""]
                        yield internal, [], files
                        return
                    dirs = {}
                    files = {}
                    for info in infos:
                        entry_internal = self._checked_strip_protocol(info["name"])
                        name = posixpath.basename(entry_internal.rstrip("/"))
                        (dirs if info["type"] == "directory" else files)[name] = dict(
                            info
                        )
                    children_by_name = {
                        name: posixpath.join(directory, name) for name in dirs
                    }
                    yielded_dirs = dirs if detail else list(dirs)
                    yielded_files = files if detail else list(files)
                    yield directory, yielded_dirs, yielded_files
                    names = yielded_dirs.keys() if detail else yielded_dirs
                    children = [
                        children_by_name[name]
                        for name in names
                        if name in children_by_name
                    ]
                    if _depth(directory) - root_depth >= descend_limit:
                        if hard_limit and children:
                            raise _oserror(errno.EFBIG, self._fullpath(internal))
                        if not hard_limit:
                            continue
                    self._prefetch_walk_children(children, on_error, budget)
                    pending.extend(reversed(children))
                return
            # Bottom-up order requires discovering all descendants before the
            # first result can be yielded, so retain the native bulk traversal.
            fill_epoch = self._dircache_epoch_snapshot()
            try:
                tree = self._native_walk_tree(internal, fill_epoch, maxdepth=maxdepth)
            except (FileNotFoundError, OSError) as error:
                if getattr(error, "errno", None) == errno.EFBIG:
                    raise
                try:
                    root_is_file = self.isfile(internal)
                except (FileNotFoundError, OSError) as probe_error:
                    if getattr(probe_error, "errno", None) == errno.EFBIG:
                        raise
                    if on_error == "raise":
                        raise probe_error
                    if callable(on_error):
                        on_error(probe_error)
                    tree = []
                else:
                    if root_is_file:
                        try:
                            info = self.info(internal)
                        except (FileNotFoundError, OSError) as probe_error:
                            if getattr(probe_error, "errno", None) == errno.EFBIG:
                                raise
                            if on_error == "raise":
                                raise probe_error
                            if callable(on_error):
                                on_error(probe_error)
                            tree = []
                        else:
                            files = {"": info} if detail else [""]
                            yield internal, [], files
                            return
                    elif on_error == "raise":
                        raise
                    else:
                        tree = self._best_effort_walk_tree(internal, maxdepth, on_error)
        by_dir = {}
        for directory, infos in tree:
            dirs = {}
            files = {}
            for info in infos:
                entry_internal = self._checked_strip_protocol(info["name"])
                name = posixpath.basename(entry_internal.rstrip("/"))
                (dirs if info["type"] == "directory" else files)[name] = dict(info)
            by_dir[directory] = (dirs, files)
        root_depth = _depth(internal)
        depth_limit = None if maxdepth is None else root_depth + maxdepth - 1
        visible = {
            directory
            for directory in by_dir
            if depth_limit is None or _depth(directory) <= depth_limit
        }
        order = []
        pending = [(internal, False)]
        while pending:
            directory, visited = pending.pop()
            if directory not in visible:
                continue
            if visited:
                order.append(directory)
                continue
            pending.append((directory, True))
            children = [
                self._checked_strip_protocol(info["name"])
                for info in by_dir.get(directory, ({}, {}))[0].values()
                if self._checked_strip_protocol(info["name"]) in visible
            ]
            pending.extend((child, False) for child in reversed(children))
        for directory in order:
            dirs, files = by_dir[directory]
            if not detail:
                dirs = list(dirs)
                files = list(files)
            yield directory, dirs, files

    def find(self, path, maxdepth=None, withdirs=False, detail=False, **kwargs):
        internal = self._checked_strip_protocol(path)
        root_depth = _depth(internal)
        out = {}
        if withdirs and internal != "":
            info = self._directory_info(internal)
            if info is not None:
                out[internal] = info
        for _, dirs, files in self.walk(
            internal, maxdepth=maxdepth, detail=True, **kwargs
        ):
            entries = list(files.values())
            if withdirs:
                entries.extend(dirs.values() if isinstance(dirs, dict) else [])
            for info in entries:
                full = self._checked_strip_protocol(info["name"])
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
        internal = self._checked_strip_protocol(path)
        root_depth = _depth(internal)
        max_entry_depth = None if maxdepth is None else root_depth + maxdepth
        sizes = {}
        if withdirs:
            info = self._directory_info(internal)
            if info is not None:
                sizes[info["name"]] = info.get("size") or 0
        for _, dirs, files in self.walk(
            internal, maxdepth=maxdepth, detail=True, **kwargs
        ):
            for info in files.values():
                entry = self._checked_strip_protocol(info["name"])
                if max_entry_depth is None or _depth(entry) <= max_entry_depth:
                    sizes[info["name"]] = info.get("size") or 0
            if withdirs:
                for info in dirs.values():
                    entry = self._checked_strip_protocol(info["name"])
                    if max_entry_depth is None or _depth(entry) <= max_entry_depth:
                        sizes[info["name"]] = info.get("size") or 0
        if not sizes and self.isfile(internal):
            info = self.info(internal)
            sizes[info["name"]] = info.get("size") or 0
        if total:
            return sum(sizes.values())
        return sizes
