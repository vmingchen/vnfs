"""fsspec filesystem implementation for the vnfs NFSv4.1 client."""

import datetime
import errno
import io
import posixpath
from glob import has_magic

from fsspec.spec import AbstractFileSystem

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
    if cls is OSError:
        return cls(errno_code, f"{msg}: {path!r}")
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
        return self.fs.size(self.path)

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
            data = data.encode()
        fd = self._ensure_open()
        if self._base_mode.startswith("a"):
            # O_APPEND: the backend appends regardless of the requested offset.
            n = self.fs._client.write(fd, data)
        else:
            n = self.fs._client.pwrite(fd, data, self._pos)
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
        if size is None:
            size = self._pos
        self.fs._client.truncate(self.fs._native_path(self.path), size)
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

    def __init__(self, host="127.0.0.1", root="", backend="nfs", dummy_root=None, **kwargs):
        super().__init__(**kwargs)
        self.host = host
        self.backend = backend
        self._root = root.strip("/")
        self._client = _native.NfsClient(host, backend, dummy_root)

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
            _info_dict(self._fullpath(self._internalize(e["name"])), e)
            for e in entries
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

    def created(self, path):
        ts = self.info(path).get("created")
        return datetime.datetime.fromtimestamp(ts) if ts is not None else None

    def modified(self, path):
        ts = self.info(path).get("modified")
        return datetime.datetime.fromtimestamp(ts) if ts is not None else None

    def ukey(self, path):
        info = self.info(path)
        return f"{info.get('fileid')}:{info.get('modified')}:{info.get('size')}"

    def checksum(self, path):
        return self.info(path).get("checksum")

    # -- bulk read / write -------------------------------------------------

    def _cat_batch(self, paths, on_error):
        """One stat_many + one read_many for a list of internal paths."""
        native = [self._native_path(p) for p in paths]
        stats, stat_errors = self._client.stat_many(native)
        ok = [i for i, s in enumerate(stats) if s is not None]
        data = [b""] * len(paths)
        read_errors = {}
        if ok:
            ok_paths = [paths[i] for i in ok]
            sizes = [stats[i]["size"] for i in ok]
            dat, read_errors = self._client.read_many(
                [self._native_path(p) for p in ok_paths], [0] * len(ok_paths), sizes
            )
            for j, i in enumerate(ok):
                data[i] = dat[j] or b""
        errors = dict(stat_errors)
        errors.update(read_errors)
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
            if len(paths) == 1:
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
        data, errors = self._client.read_many(
            [self._native_path(internal)], [start if start is not None else 0], [end]
        )
        if errors:
            raise _oserror(errors[0], internal)
        return data[0]

    def cat_ranges(self, paths, starts, ends, max_gap=None, on_error="return", **kwargs):
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
        data, errors = self._client.read_many(
            [self._native_path(p) for p in internals], starts, ends
        )
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
        try:
            self._client.write_many(native, datas)
        except FileNotFoundError:
            # Like LocalFileSystem(auto_mkdir=True), create missing parents
            # once, then retry (only pays round trips when a parent is absent).
            for parent in sorted({posixpath.dirname(p) for p in paths}):
                if parent not in ("", "/"):
                    self._client.ensure_dir(self._native_path(parent), 0o755)
            self._client.write_many(native, datas)
        # Truncate to the written length (removes a stale tail on overwrite).
        self._client.truncate_many(native, [len(d) for d in datas])

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

    # -- open / file objects ----------------------------------------------

    def _open(self, path, mode="rb", block_size=None, autocommit=True, cache_options=None, **kwargs):
        return Nfs4File(self, self._strip_protocol(path), mode)

    def open_many(self, open_files):
        """Open a list of ``OpenFile`` objects in one openv batch."""
        paths = [self._strip_protocol(f.path) for f in open_files]
        modes = [f.mode for f in open_files]
        fds = self._client.open_many([self._native_path(p) for p in paths], modes)
        return [Nfs4File(self, p, m, fd=fd) for p, m, fd in zip(paths, modes, fds)]

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
        native = self._native_path(internal)
        if create_parents:
            self._client.ensure_dir(native, mode)
        else:
            self._client.mkdir(native, mode)

    def makedirs(self, path, exist_ok=False):
        internal = self._strip_protocol(path)
        if self.exists(internal):
            if exist_ok and self.isdir(internal):
                return
            raise FileExistsError(errno.EEXIST, f"File exists: {internal!r}")
        self._client.ensure_dir(self._native_path(internal), 0o755)

    def rmdir(self, path):
        self._client.remove_many([self._native_path(self._strip_protocol(path))])

    def rm(self, path, recursive=False, maxdepth=None):
        if isinstance(path, str):
            paths = [self._strip_protocol(path)]
        else:
            paths = [self._strip_protocol(p) for p in path]
        if recursive:
            self._client.rm([self._native_path(p) for p in paths], True)
        else:
            self._client.remove_many([self._native_path(p) for p in paths])

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
            pairs = [(self._strip_protocol(path1), self._strip_protocol(path2))]
        self._client.rename_many(
            [(self._native_path(a), self._native_path(b)) for a, b in pairs]
        )

    def cp_file(self, path1, path2, **kwargs):
        """Copy a single file (or create a directory) between two paths."""
        src = self._strip_protocol(path1)
        dst = self._strip_protocol(path2)
        if self.isdir(src):
            self._client.ensure_dir(self._native_path(dst), 0o755)
            return
        if not self.isfile(src):
            raise FileNotFoundError(src)
        parent = self._parent(dst)
        if parent not in ("", "/"):
            self._client.ensure_dir(self._native_path(parent), 0o755)
        self._copy_pairs([(src, dst)], "raise")

    def copy(self, path1, path2, recursive=False, maxdepth=None, on_error=None, **kwargs):
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
            if self.isdir(src) and not self.isdir(dst):
                self._client.cp_recursive(
                    self._native_path(src), self._native_path(dst), False
                )
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
        native_pairs = [
            (self._native_path(a), self._native_path(b)) for a, b in pairs
        ]
        copied, errors = self._client.copy_many(native_pairs)
        if errors and all(err == 2 for err in errors.values()):
            # Missing destination parents (matching cp_file's auto_mkdir):
            # create them once, then retry.
            parents = {posixpath.dirname(b) for _, b in native_pairs}
            for parent in sorted(parents):
                if parent not in ("", "/"):
                    self._client.ensure_dir(parent, 0o755)
            copied, errors = self._client.copy_many(native_pairs)
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
        self._client.symlink(target, self._native_path(self._strip_protocol(path)))

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
                files = {"" : info} if detail else [""]
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
            d
            for d in by_dir
            if maxdepth is None or _depth(d) <= root_depth + maxdepth
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
        for _, dirs, files in self.walk(internal, maxdepth=maxdepth, detail=True, **kwargs):
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
