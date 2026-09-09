"""fsspec filesystem implementation for the vectorized VFSI backends."""

import datetime
import errno
import hashlib
import io
import math
import os
import posixpath
import tempfile
import uuid
from glob import has_magic
from urllib.parse import unquote, urlsplit

from fsspec.callbacks import DEFAULT_CALLBACK
from fsspec.spec import AbstractFileSystem
from fsspec.transaction import Transaction

from . import _native

__all__ = ["Nfs4File", "Nfs4FileSystem", "VfsiFileSystem"]


def _normalize_mode(mode):
    """Collapse 'b'/'t' characters: 'rt'/'r'/'rb' -> 'r', 'wb+' -> 'w+', ..."""
    base = mode.replace("b", "").replace("t", "")
    if base not in ("r", "r+", "w", "w+", "a", "a+", "x", "x+"):
        raise ValueError(f"unsupported mode: {mode!r}")
    return base


def _oserror(errno_code, path):
    """Rebuild a Python exception from a native errno (for batched results)."""
    if errno_code == _native.ERR_UNSUPPORTED:
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
    return {k: v for k, v in info.items() if v is not None}


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
    if end is not None and end < start:
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

    def __init__(self, factory_args, auto_reconnect=True):
        self._factory_args = factory_args
        self._auto_reconnect = auto_reconnect
        self._native = _native.NfsClient(*factory_args)
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
        self._native = _native.NfsClient(*self._factory_args)
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


class Nfs4File(io.RawIOBase):
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
        if self._fd is not None and self._fd_generation != self.fs._client.generation:
            self._fd = None
            self._fd_generation = None
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

    def _pread(self, length):
        """Retry an absolute-offset read once on a fresh session."""
        fd = self._ensure_open()
        try:
            return self.fs._client.pread(fd, length, self._pos)
        except ConnectionError:
            if not self.fs.auto_reconnect:
                raise
            self.fs._client.reconnect()
            self._fd = None
            self._fd_generation = None
            fd = self._ensure_open()
            return self.fs._client.pread(fd, length, self._pos)

    def _size(self):
        if self._cached_size is None:
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
        data = self._pread(min(len(b), self._MAX_READ))
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
                if self.fs.auto_reconnect:
                    self.fs._client.reconnect()
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
            raise ValueError("negative seek position")
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
        try:
            if (
                fd is not None
                and self._fd_generation == self.fs._client.generation
                and not self.fs._client.closed
            ):
                self.fs._client.close(fd)
        finally:
            self._fd = None
            self._fd_generation = None
            self._closed = True
            super().close()

    @property
    def size(self):
        return self._size()


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
            if self.fs is not None:
                self.fs._intrans = False
                self.fs._transaction = None
            self.fs = None


class Nfs4FileSystem(AbstractFileSystem):
    """An fsspec filesystem over a vectorized VFSI backend.

    Parameters
    ----------
    host: str
        NFS server host (default 127.0.0.1).
    root: str
        Export-relative prefix ("chroot") all paths are resolved under, e.g.
        ``"git/vnfs_tests"``.
    backend: "nfs", "smb", or "dummy"
        ``dummy`` uses a local-directory implementation of the same vectorized
        API. ``smb`` connects to the SMB2/3 share named by ``share``.
    dummy_root: str or None
        Filesystem root for the dummy backend (a unique temp dir when None).
    """

    protocol = "nfs4"
    root_marker = "/"
    transaction_type = _VfsiTransaction

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
        connect_timeout=10.0,
        request_timeout=5.0,
        auto_reconnect=True,
        **kwargs,
    ):
        if backend not in {"nfs", "smb", "dummy"}:
            raise ValueError("backend must be 'nfs', 'smb', or 'dummy'")
        if backend != "dummy" and not host:
            raise ValueError("host must not be empty for network backends")
        if compound_size_limit is not None and compound_size_limit <= 0:
            raise ValueError("compound_size_limit must be a positive integer")
        if minor_version not in (None, 1, 2):
            raise ValueError("minor_version must be 1, 2, or None")
        for name, value in (
            ("batch_size", batch_size),
            ("max_batch_bytes", max_batch_bytes),
            ("transfer_chunk_size", transfer_chunk_size),
            ("transaction_spool_threshold", transaction_spool_threshold),
        ):
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise ValueError(f"{name} must be a positive integer")
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
        super().__init__(**kwargs)
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
        self.batch_size = batch_size
        self.max_batch_bytes = max_batch_bytes
        self.transfer_chunk_size = transfer_chunk_size
        self.transaction_spool_threshold = transaction_spool_threshold
        self.connect_timeout = float(connect_timeout)
        self.request_timeout = float(request_timeout)
        self.auto_reconnect = bool(auto_reconnect)
        self._root = root.strip("/")
        self._client = _ResilientClient(
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
            ),
            auto_reconnect=self.auto_reconnect,
        )

    @property
    def closed(self):
        """Whether this filesystem's native session has been released."""
        return self._client.closed

    def close(self):
        """Release the native connection and evict this cached instance."""
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

    def ls(self, path, detail=True, **kwargs):
        internal = self._strip_protocol(path)
        try:
            entries = self._client.listdir(self._native_path(internal))
        except NotADirectoryError:
            # The path is a file (or symlink to one): report it directly.
            info = self.info(internal, **kwargs)
            return [info] if detail else [info["name"]]
        infos = [
            _info_dict(self._fullpath(self._internalize(e["name"])), e) for e in entries
        ]
        infos.sort(key=lambda d: d["name"])
        if detail:
            return infos
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
        info = self.info(path)
        # Like LocalFileSystem (a hash of `info`, which includes the name),
        # the key changes when the file is renamed as well as when its
        # contents change.
        return hashlib.sha256(
            f"{info.get('name')}:{self._version_token(info)}".encode()
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

    def _cat_batch(self, paths, on_error):
        """Read path groups bounded by both item count and aggregate bytes."""
        native = [self._native_path(path) for path in paths]
        stats, stat_errors = self._client.stat_many(native)
        data = [b""] * len(paths)
        failures = {
            index: _oserror(code, paths[index]) for index, code in stat_errors.items()
        }
        valid = [index for index in range(len(paths)) if index not in failures]
        valid_sizes = [stats[index].get("size", 0) for index in valid]
        for positions in _bounded_batches(
            valid_sizes, self.batch_size, self.max_batch_bytes
        ):
            batch = [valid[position] for position in positions]
            if len(batch) == 1 and valid_sizes[positions[0]] > self.max_batch_bytes:
                index = batch[0]
                try:
                    data[index] = self._read_one_streamed(paths[index])
                except OSError as exc:
                    failures[index] = exc
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

    def _read_one_streamed(self, path):
        """Read one result incrementally when it exceeds the batch byte cap."""
        output = io.BytesIO()
        self._copy_remote_to_fileobj(path, output)
        return output.getvalue()

    def cat(self, path, recursive=False, on_error="raise", **kwargs):
        if isinstance(path, str):
            paths = self.expand_path(path, recursive=recursive, **kwargs)
            if len(paths) == 1 and paths[0] == self._strip_protocol(path):
                # Single literal path: cat_file semantics (raise on error).
                return self.cat_file(paths[0], **kwargs)
            return self._cat_batch(paths, on_error)
        paths = [self._strip_protocol(p) for p in path]
        if recursive:
            expanded = []
            for p in paths:
                expanded.extend(self.find(p, withdirs=False))
            paths = expanded
        return self._cat_batch(paths, on_error)

    def cat_file(self, path, start=None, end=None, **kwargs):
        internal = self._strip_protocol(path)
        size = None
        if (start is not None and start < 0) or (end is not None and end < 0):
            size = self.size(internal)
        start, end = _normalize_range(start, end, size)
        if start == 0 and end is None:
            # Whole file: read_allv skips the size stat entirely.
            data, errors = self._client.read_all_many([self._native_path(internal)])
            if errors:
                raise _oserror(errors[0], internal)
            return data[0] or b""
        data, errors = self._client.read_many(
            [self._native_path(internal)], [start], [end]
        )
        if errors:
            raise _oserror(errors[0], internal)
        return data[0]

    def cat_ranges(
        self, paths, starts, ends, max_gap=None, on_error="return", **kwargs
    ):
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
        internals = [self._strip_protocol(p) for p in paths]
        native = [self._native_path(p) for p in internals]
        errors = {}
        sizes = [None] * len(paths)
        if any(
            (s is not None and s < 0) or e is None or e < 0
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
            elif i in errors:
                exc = _oserror(errors[i], p)
                if on_error == "return":
                    out.append(exc)
                else:
                    raise exc
            else:
                out.append(data[i] or b"")
        return out

    def _write_batch(self, paths, values, mode="overwrite"):
        native = [self._native_path(p) for p in paths]
        if mode == "create":
            for path, value in zip(paths, values):
                self._write_one_exclusive(path, value)
            return
        if mode != "overwrite":
            raise ValueError("mode must be 'overwrite' or 'create'")
        datas = [v if isinstance(v, bytes) else bytes(v) for v in values]
        for batch in _bounded_batches(
            [len(data) for data in datas], self.batch_size, self.max_batch_bytes
        ):
            if len(batch) == 1 and len(datas[batch[0]]) > self.max_batch_bytes:
                self._write_one_streamed(paths[batch[0]], datas[batch[0]])
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

    def _write_one_streamed(self, path, value):
        """Overwrite one in-memory value with bounded native write calls."""
        internal = self._strip_protocol(path)
        parent = posixpath.dirname(internal)
        if self.auto_mkdir and parent not in ("", "/"):
            self._ensure_dirs([parent], 0o755)
        view = memoryview(value)
        with Nfs4File(self, internal, "wb") as remote:
            for offset in range(0, len(view), self.transfer_chunk_size):
                remote.write(bytes(view[offset : offset + self.transfer_chunk_size]))

    def _write_one_exclusive(self, path, value):
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
        with Nfs4File(self, path, "xb") as remote:
            while True:
                chunk = spool.read(self.transfer_chunk_size)
                if not chunk:
                    break
                remote.write(chunk)

    def _copy_remote_to_fileobj(self, path, output):
        """Stream one remote file into a writable local file object."""
        with self.open(path, "rb") as remote:
            while True:
                chunk = remote.read(self.transfer_chunk_size)
                if not chunk:
                    break
                output.write(chunk)

    def pipe(self, path, value=None, **kwargs):
        if isinstance(path, str):
            paths = [self._strip_protocol(path)]
            values = [value if value is not None else b""]
        elif isinstance(path, dict):
            paths = [self._strip_protocol(k) for k in path]
            values = list(path.values())
        else:
            raise ValueError("path must be str or dict")
        self._write_batch(paths, values, kwargs.get("mode", "overwrite"))

    def pipe_file(self, path, value, mode="overwrite", **kwargs):
        self._write_batch([self._strip_protocol(path)], [value], mode)

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
                callback.relative_update(0)
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
        for batch in _bounded_batches(
            [size for _, _, size in pairs], self.batch_size, self.max_batch_bytes
        ):
            if len(batch) == 1 and pairs[batch[0]][2] > self.max_batch_bytes:
                remote_path, local_path, size = pairs[batch[0]]
                with callback.branched(remote_path, local_path) as child:
                    child.set_size(size)
                    local.makedirs(local._parent(local_path), exist_ok=True)
                    with open(local_path, "wb") as out:
                        with self.open(remote_path, "rb") as remote:
                            while True:
                                chunk = remote.read(self.transfer_chunk_size)
                                if not chunk:
                                    break
                                out.write(chunk)
                                child.relative_update(len(chunk))
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
                callback.relative_update(0)
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

    # -- open / file objects ----------------------------------------------

    def _open(
        self,
        path,
        mode="rb",
        block_size=None,
        autocommit=True,
        cache_options=None,
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
        return Nfs4File(self, internal, mode)

    def open_many(self, open_files):
        """Open a list of ``OpenFile`` objects in one openv batch."""
        paths = [self._strip_protocol(f.path) for f in open_files]
        modes = [f.mode for f in open_files]
        if self.auto_mkdir and any(any(c in m for c in "wax") for m in modes):
            parents = {
                posixpath.dirname(p) for p in paths if posixpath.dirname(p) != "/"
            }
            self._ensure_dirs(list(parents), 0o755)
        fds = self._client.open_many([self._native_path(p) for p in paths], modes)
        files = [Nfs4File(self, p, m, fd=fd) for p, m, fd in zip(paths, modes, fds)]
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
            self._ensure_dirs([internal], mode)
        else:
            self._client.mkdir(self._native_path(internal), mode)

    def makedirs(self, path, exist_ok=False):
        self._makedirs_batched([path], exist_ok)

    def rmdir(self, path):
        self._client.remove_many([self._native_path(self._strip_protocol(path))])

    def rm(self, path, recursive=False, maxdepth=None):
        if isinstance(path, str):
            paths = [self._strip_protocol(path)]
        else:
            paths = [self._strip_protocol(p) for p in path]
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
                    raise IsADirectoryError(
                        errno.EISDIR, f"Is a directory: {paths[i]!r}"
                    )
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
        self._client.rename_many(
            [(self._native_path(a), self._native_path(b)) for a, b in pairs]
        )

    def _copy_recursive(self, src, dst, symlinks=False):
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

        self._ensure_dirs(list(dest_dirs), 0o755)
        if pairs:
            native_pairs = [
                (self._native_path(a), self._native_path(b)) for a, b in pairs
            ]
            _, errors = self._client.copy_many(native_pairs)
            if errors:
                i = min(errors)
                raise _oserror(errors[i], pairs[i][0])
        for s, d in symlink_pairs:
            target = self.readlink(s)
            self.symlink(target, d)

    def cp_file(self, path1, path2, **kwargs):
        """Copy a single file (or create a directory) between two paths."""
        src = self._strip_protocol(path1)
        dst = self._strip_protocol(path2)
        info = self.info(src)
        if info["type"] == "directory":
            self._ensure_dirs([dst], 0o755)
            return
        if info["type"] != "file":
            raise FileNotFoundError(src)
        parent = self._parent(dst)
        if self.auto_mkdir and parent not in ("", "/"):
            self._ensure_dirs([parent], 0o755)
        self._copy_pairs([(src, dst)], "raise")

    def copy(
        self, path1, path2, recursive=False, maxdepth=None, on_error=None, **kwargs
    ):
        if on_error is None:
            on_error = "ignore" if recursive else "raise"
        if isinstance(path1, list) and isinstance(path2, list):
            if len(path1) != len(path2):
                raise ValueError("path1 and path2 must be lists of equal length")
            pairs = [
                (self._strip_protocol(a), self._strip_protocol(b))
                for a, b in zip(path1, path2)
            ]
            self._copy_pairs(pairs, on_error)
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
                self._copy_recursive(src, dst, kwargs.get("symlinks", False))
                return
        # Everything else (globs, trailing-slash semantics, maxdepth, missing
        # parents) follows the base implementation, which resolves paths via
        # expand_path/other_paths and calls cp_file per pair.
        return super().copy(
            path1,
            path2,
            recursive=recursive,
            maxdepth=maxdepth,
            on_error=on_error,
            **kwargs,
        )

    cp = copy

    def _copy_pairs(self, pairs, on_error):
        if not pairs:
            return
        native_pairs = [(self._native_path(a), self._native_path(b)) for a, b in pairs]
        _, errors = self._client.copy_many(native_pairs)
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
                _, errors = self._client.copy_many(native_pairs)
        if errors:
            i = min(errors)
            exc = _oserror(errors[i], pairs[i][0])
            if on_error == "raise":
                raise exc

    def touch(self, path, truncate=True, **kwargs):
        internal = self._strip_protocol(path)
        if not truncate and self.exists(internal):
            raise NotImplementedError("timestamp updates are not supported")
        self._write_batch([internal], [b""])

    def symlink(self, target, path, **kwargs):
        link = self._native_path(self._strip_protocol(path))
        if target.startswith(("/", "nfs4://", "nfs4::", "vfsi://", "vfsi::")):
            # Absolute targets are relative to the filesystem root (chroot
            # semantics, matching LocalFileSystem's OS-absolute targets);
            # map them through the root prefix. Relative targets are stored
            # as-is and resolve relative to the link's directory.
            target = self._native_path(self._strip_protocol(target))
        self._client.symlink(target, link)

    def readlink(self, path):
        return self._client.readlink(self._native_path(self._strip_protocol(path)))

    def hardlink(self, src, dst):
        self._client.hardlink(
            self._native_path(self._strip_protocol(src)),
            self._native_path(self._strip_protocol(dst)),
        )

    # -- traversal ---------------------------------------------------------

    def walk(self, path, maxdepth=None, topdown=True, on_error="omit", **kwargs):
        if maxdepth is not None and maxdepth < 1:
            raise ValueError("maxdepth must be at least 1")
        detail = kwargs.pop("detail", False)
        internal = self._strip_protocol(path)
        try:
            tree = self._client.walk(self._native_path(internal), sort=True)
        except (FileNotFoundError, OSError) as e:
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
        for dir_path, entries in tree:
            dirs = {}
            files = {}
            for e in entries:
                entry_internal = self._internalize(e["name"])
                name = posixpath.basename(entry_internal.rstrip("/"))
                info = _info_dict(self._fullpath(entry_internal), e)
                (dirs if e["type"] == "directory" else files)[name] = info
            by_dir[self._internalize(dir_path)] = (dirs, files)
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
        out = {}
        if withdirs and internal != "" and self.isdir(internal):
            out[internal] = self.info(internal)
        try:
            tree = self._client.walk(self._native_path(internal), sort=True)
        except (FileNotFoundError, OSError):
            tree = []
        root_depth = _depth(internal)
        for dir_path, entries in tree:
            for e in entries:
                full = self._internalize(e["name"])
                if maxdepth is not None and _depth(full) > root_depth + maxdepth:
                    continue
                if e["type"] == "directory" and not withdirs:
                    continue
                out[full] = _info_dict(self._fullpath(full), e)
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


class VfsiFileSystem(Nfs4FileSystem):
    """Protocol-neutral alias for selecting NFS, SMB, or dummy backends."""

    protocol = "vfsi"
