"""fsspec filesystem implementation for the vnfs NFSv4.1 client."""

import datetime
import errno
import hashlib
import io
import os
import posixpath
from glob import has_magic

from fsspec.spec import AbstractFileSystem
from fsspec.callbacks import DEFAULT_CALLBACK

from . import _native

__all__ = ["Nfs4File", "Nfs4FileSystem"]


def _normalize_mode(mode):
    """Collapse 'b'/'t' characters: 'rt'/'r'/'rb' -> 'r', 'wb+' -> 'w+', ..."""
    base = mode.replace("b", "").replace("t", "")
    if base not in ("r", "r+", "w", "w+", "a", "a+", "x", "x+"):
        raise ValueError(f"unsupported mode: {mode!r}")
    return base


def _oserror(errno_code, path):
    """Rebuild a Python exception from a native errno (for batched results)."""
    table = {
        2: (FileNotFoundError, "No such file or directory"),
        13: (PermissionError, "Permission denied"),
        17: (FileExistsError, "File exists"),
        20: (NotADirectoryError, "Not a directory"),
        21: (IsADirectoryError, "Is a directory"),
    }
    cls, msg = table.get(errno_code, (OSError, f"errno {errno_code}"))
    return cls(errno_code, f"{msg}: {path!r}")


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
        "created": attrs.get("created"),
        "modified": attrs.get("modified"),
        "checksum": attrs.get("checksum"),
        "islink": attrs.get("islink", False),
    }
    return {k: v for k, v in info.items() if v is not None}


def _depth(path):
    """Number of components below the root for an internal path."""
    if path == "/":
        return 0
    return path.count("/")


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
        if self._fd is None:
            self._fd = self.fs._client.open(
                self.fs._native_path(self.path), self._native_mode()
            )
        return self._fd

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
        fd = self._ensure_open()
        data = self.fs._client.pread(fd, min(len(b), self._MAX_READ), self._pos)
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
        fd = self._ensure_open()
        chunks = []
        remaining = size
        while remaining > 0:
            chunk = self.fs._client.pread(fd, min(remaining, self._MAX_READ), self._pos)
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
        if self._base_mode.startswith("a"):
            # O_APPEND: the backend appends regardless of the requested offset.
            n = self.fs._client.write(fd, data)
        else:
            n = self.fs._client.pwrite(fd, data, self._pos)
        if self._cached_size is not None:
            self._cached_size = max(self._cached_size, self._pos + n)
        self._pos += n
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
            if fd is not None:
                self.fs._client.close(fd)
        finally:
            self._fd = None
            self._closed = True
            super().close()

    @property
    def size(self):
        return self._size()


class _DeferredWriteFile:
    """Write-only buffer that lands on the filesystem only at ``commit()``.

    fsspec transaction semantics (see ``Transaction`` and
    ``test_local.py::test_commit_discard``): a file opened inside a
    transaction must not exist until the transaction completes; on a normal
    exit it is committed, on an exception it is discarded.
    """

    def __init__(self, fs, path, mode="wb"):
        self.fs = fs
        self.path = path  # internal path
        self.mode = mode
        self._buffer = bytearray()
        self._pos = 0
        self._closed = False
        self._committed = False

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
        n = len(data)
        end = self._pos + n
        if end > len(self._buffer):
            self._buffer.extend(b"\0" * (end - len(self._buffer)))
        self._buffer[self._pos : end] = data
        self._pos = end
        return n

    def seek(self, offset, whence=0):
        if whence == 0:
            new = offset
        elif whence == 1:
            new = self._pos + offset
        elif whence == 2:
            new = len(self._buffer) + offset
        else:
            raise ValueError(f"invalid whence: {whence}")
        if new < 0:
            raise ValueError("negative seek position")
        self._pos = new
        return new

    def tell(self):
        return self._pos

    def flush(self):
        return None

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def close(self):
        # The buffer is retained: the target is written at commit().
        self._closed = True

    def commit(self):
        if self._committed:
            return
        self.fs._write_batch([self.path], [bytes(self._buffer)])
        self._committed = True
        self._closed = True

    def discard(self):
        self._buffer.clear()
        self._committed = True
        self._closed = True

    @property
    def closed(self):
        return self._closed


class Nfs4FileSystem(AbstractFileSystem):
    """An fsspec filesystem over the vectorized vnfs NFSv4.1 client.

    Parameters
    ----------
    host: str
        NFS server host (default 127.0.0.1).
    root: str
        Export-relative prefix ("chroot") all paths are resolved under, e.g.
        ``"git/vnfs_tests"``.
    backend: "nfs" or "dummy"
        ``dummy`` uses a local-directory implementation of the same vectorized
        API (for tests and development without a server).
    dummy_root: str or None
        Filesystem root for the dummy backend (a unique temp dir when None).
    """

    protocol = "nfs4"
    root_marker = "/"

    def __init__(
        self,
        host="127.0.0.1",
        root="",
        backend="nfs",
        dummy_root=None,
        auto_mkdir=False,
        compound_size_limit=None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self.host = host
        self.backend = backend
        # Like LocalFileSystem: write-mode operations create missing parent
        # directories only when auto_mkdir is set.
        self.auto_mkdir = auto_mkdir
        # Per-compound payload cap (bytes) for merged path I/O; None uses the
        # native default (1 MiB).
        self.compound_size_limit = compound_size_limit
        self._root = root.strip("/")
        self._client = _native.NfsClient(host, backend, dummy_root, compound_size_limit)

    # -- path handling -----------------------------------------------------

    @classmethod
    def _strip_protocol(cls, path):
        if isinstance(path, list):
            return [cls._strip_protocol(p) for p in path]
        if not isinstance(path, str):
            path = str(path)
        protos = (cls.protocol,) if isinstance(cls.protocol, str) else cls.protocol
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
        """Internal ('/a/b') -> full fsspec path ('nfs4:///a/b')."""
        return "nfs4://" + internal

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
        except Exception:
            return False

    def isfile(self, path):
        try:
            return self.info(path)["type"] == "file"
        except Exception:
            return False

    def isdir(self, path):
        try:
            return self.info(path)["type"] == "directory"
        except OSError:
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
            f"{info.get('name')}:{info.get('fileid')}:{info.get('modified')}:{info.get('size')}".encode()
        ).hexdigest()

    def checksum(self, path):
        # A stable, content-sensitive value: unlike the raw fileid, it
        # changes when the file's contents change (mtime/size).
        info = self.info(path)
        digest = hashlib.sha256(
            f"{info.get('fileid')}:{info.get('modified')}:{info.get('size')}".encode()
        ).hexdigest()
        return int(digest, 16)

    # -- bulk read / write -------------------------------------------------

    def _cat_batch(self, paths, on_error):
        """One read_allv batch (no per-file size stats) for internal paths."""
        native = [self._native_path(p) for p in paths]
        data = [b""] * len(paths)
        dat, errors = self._client.read_all_many(native)
        for i, d in enumerate(dat):
            if d is not None:
                data[i] = d
        out = {}
        for i, p in enumerate(paths):
            if i in errors:
                exc = _oserror(errors[i], p)
                if on_error == "raise":
                    raise exc
                if on_error == "return":
                    out[p] = exc
                # "omit": skip the failed key
            else:
                out[p] = data[i]
        return out

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
        if (start is not None and start < 0) or (end is not None and end < 0):
            # Slice semantics: negative bounds are offsets from the end.
            size = self.size(internal)
            if start is not None and start < 0:
                start = max(0, size + start)
            if end is not None and end < 0:
                end = size + end
        if start in (None, 0) and end is None:
            # Whole file: read_allv skips the size stat entirely.
            data, errors = self._client.read_all_many([self._native_path(internal)])
            if errors:
                raise _oserror(errors[0], internal)
            return data[0] or b""
        data, errors = self._client.read_many(
            [self._native_path(internal)], [start if start is not None else 0], [end]
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
        if any(
            (s is not None and s < 0) or (e is not None and e < 0)
            for s, e in zip(starts, ends)
        ):
            # Slice semantics: resolve negative bounds with one size fetch.
            stats, stat_errors = self._client.stat_many(native)
            errors.update(stat_errors)
            sizes = [s["size"] if s is not None else 0 for s in stats]
            starts = [
                max(0, s + sizes[i]) if (s is not None and s < 0) else s
                for i, s in enumerate(starts)
            ]
            ends = [
                sizes[i] + e if (e is not None and e < 0) else e
                for i, e in enumerate(ends)
            ]
        data, read_errors = self._client.read_many(native, starts, ends)
        errors.update(read_errors)
        out = []
        for i, p in enumerate(internals):
            if i in errors:
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
            existing = self._client.exists_many(native)
            for i, e in enumerate(existing):
                if e:
                    raise FileExistsError(errno.EEXIST, f"File exists: {paths[i]!r}")
        datas = [v if isinstance(v, bytes) else bytes(v) for v in values]
        truncate = mode != "create"
        try:
            self._client.write_many(native, datas, truncate=truncate)
        except FileNotFoundError:
            if not self.auto_mkdir:
                raise
            # Create missing parents once, then retry (only pays round trips
            # when a parent is absent).
            parents = {
                posixpath.dirname(p) for p in paths if posixpath.dirname(p) != "/"
            }
            self._ensure_dirs(list(parents), 0o755)
            self._client.write_many(native, datas, truncate=truncate)

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
        for i, (r, l) in enumerate(zip(rpaths, lpaths)):
            if stats[i] is not None and stats[i]["type"] == "directory":
                local.makedirs(l, exist_ok=True)
                callback.relative_update(0)
            else:
                pairs.append((r, l))
        if not pairs:
            return
        native = [self._native_path(self._strip_protocol(r)) for r, _ in pairs]
        data, errors = self._client.read_all_many(native)
        for i, ((r, l), buf) in enumerate(zip(pairs, data)):
            with callback.branched(r, l) as child:
                if buf is None:
                    raise _oserror(errors[i], r)
                local.makedirs(local._parent(l), exist_ok=True)
                with open(l, "wb") as out:
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
        for l, r in zip(lpaths, rpaths):
            if os.path.isdir(l):
                remote_dirs.append(r)
                callback.relative_update(0)
            else:
                pairs.append((l, r))
        if remote_dirs:
            self._makedirs_batched(remote_dirs, exist_ok=True)
        if not pairs:
            return
        datas = []
        for l, _ in pairs:
            with open(l, "rb") as fh:
                datas.append(fh.read())
        remote_paths = [self._strip_protocol(r) for _, r in pairs]
        mode = kwargs.get("mode", "overwrite")
        self._write_batch(remote_paths, datas, mode=mode)
        for (l, r), buf in zip(pairs, datas):
            with callback.branched(l, r) as child:
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
        if target.startswith(("/", "nfs4://", "nfs4::")):
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
