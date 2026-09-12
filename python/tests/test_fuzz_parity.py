"""Property-based differential tests against fsspec's LocalFileSystem.

The local implementation is the behavioral oracle.  Each generated operation
is applied to an isolated local tree and to nfs4fs's dummy backend, then both
the result and the complete user-visible tree are compared.  Hypothesis
shrinks a failure to the smallest useful sequence for a permanent regression
test.
"""

import io
import os
import tempfile
from unittest.mock import patch

import fsspec
from fsspec.core import OpenFile, OpenFiles
from hypothesis import HealthCheck, settings
from hypothesis import strategies as st
from hypothesis.stateful import RuleBasedStateMachine, invariant, precondition, rule

_FILES = ("/a", "/b", "/d0/a", "/d0/b", "/d1/a", "/d0/sub/a")
_DIRS = ("/d0", "/d1", "/d0/sub")
_BLOBS = st.binary(max_size=64)
_FUZZ_EXAMPLES = int(os.environ.get("NFS4FS_FUZZ_EXAMPLES", "40"))
_FUZZ_STEPS = int(os.environ.get("NFS4FS_FUZZ_STEPS", "30"))


def _error_signature(error):
    """Compare public exception categories without backend-specific messages."""
    return type(error).__name__


class LocalOracleStateMachine(RuleBasedStateMachine):
    """Run identical filesystem histories against local and dummy VFSI."""

    def __init__(self):
        super().__init__()
        self._tmp = tempfile.TemporaryDirectory(prefix="nfs4fs-fuzz-")
        self._local_root = os.path.join(self._tmp.name, "local")
        os.mkdir(self._local_root)
        self.local = fsspec.filesystem("file", skip_instance_cache=True)
        self.nfs = fsspec.filesystem(
            "nfs4",
            backend="dummy",
            dummy_root=os.path.join(self._tmp.name, "dummy"),
            skip_instance_cache=True,
            use_listings_cache=True,
            listings_expiry_time=3600,
            max_paths=8,
            block_size=8,
        )

    def teardown(self):
        self.nfs.close()
        self._tmp.cleanup()
        super().teardown()

    def _path(self, backend, logical):
        if backend == "local":
            return self._local_root + logical
        return "nfs4://" + logical

    def _logical(self, backend, path):
        if backend == "local":
            relative = os.path.relpath(path, self._local_root)
            return "/" if relative == "." else "/" + relative.replace(os.sep, "/")
        stripped = self.nfs._strip_protocol(path)
        return "/" if stripped == "/" else "/" + stripped.lstrip("/")

    def _capture(self, call, normalize=lambda value: value):
        try:
            return "ok", normalize(call())
        except Exception as error:  # the exception type is part of the contract
            return "error", _error_signature(error)

    def _compare(self, operation, local_call, nfs_call, normalize=lambda value: value):
        local = self._capture(local_call, normalize)
        nfs = self._capture(nfs_call, normalize)
        assert nfs == local, f"{operation}: local={local!r}, nfs4fs={nfs!r}"

    def _snapshot(self, backend):
        fs = self.local if backend == "local" else self.nfs
        root = self._local_root if backend == "local" else "nfs4:///"
        found = fs.find(root, withdirs=True, detail=True)
        snapshot = {}
        for path, info in found.items():
            logical = self._logical(backend, path)
            if logical == "/":
                continue
            kind = info["type"]
            if kind == "file":
                contents = fs.cat_file(self._path(backend, logical))
                snapshot[logical] = (kind, len(contents), contents)
            else:
                snapshot[logical] = (kind, None, None)
        return snapshot

    @invariant()
    def trees_match(self):
        assert self._snapshot("nfs") == self._snapshot("local")

    @rule(directory=st.sampled_from(_DIRS), create_parents=st.booleans())
    def mkdir(self, directory, create_parents):
        self._compare(
            f"mkdir({directory!r}, create_parents={create_parents!r})",
            lambda: self.local.mkdir(
                self._path("local", directory), create_parents=create_parents
            ),
            lambda: self.nfs.mkdir(
                self._path("nfs", directory), create_parents=create_parents
            ),
            lambda _: None,
        )

    @rule(directory=st.sampled_from(_DIRS), exist_ok=st.booleans())
    def makedirs(self, directory, exist_ok):
        self._compare(
            f"makedirs({directory!r}, exist_ok={exist_ok!r})",
            lambda: self.local.makedirs(
                self._path("local", directory), exist_ok=exist_ok
            ),
            lambda: self.nfs.makedirs(self._path("nfs", directory), exist_ok=exist_ok),
            lambda _: None,
        )

    @rule(path=st.sampled_from(_FILES), data=_BLOBS)
    def pipe_file(self, path, data):
        self._compare(
            f"pipe_file({path!r}, {data!r})",
            lambda: self.local.pipe_file(self._path("local", path), data),
            lambda: self.nfs.pipe_file(self._path("nfs", path), data),
            lambda _: None,
        )

    @rule(
        path=st.sampled_from(_FILES),
        data=_BLOBS,
        mode=st.sampled_from(("wb", "ab", "xb")),
        buffering=st.booleans(),
    )
    def open_write(self, path, data, mode, buffering):
        def write(fs, backend):
            kwargs = {"write_buffering": buffering} if backend == "nfs" else {}
            with fs.open(self._path(backend, path), mode, **kwargs) as handle:
                assert handle.write(data) == len(data)

        self._compare(
            f"open_write({path!r}, {mode!r}, buffering={buffering!r})",
            lambda: write(self.local, "local"),
            lambda: write(self.nfs, "nfs"),
            lambda _: None,
        )

    @rule(path=st.sampled_from(_FILES), truncate=st.booleans())
    def touch(self, path, truncate):
        self._compare(
            f"touch({path!r}, truncate={truncate!r})",
            lambda: self.local.touch(self._path("local", path), truncate=truncate),
            lambda: self.nfs.touch(self._path("nfs", path), truncate=truncate),
            lambda _: None,
        )

    @rule(path=st.sampled_from(_FILES + _DIRS), recursive=st.booleans())
    def remove(self, path, recursive):
        self._compare(
            f"rm({path!r}, recursive={recursive!r})",
            lambda: self.local.rm(self._path("local", path), recursive=recursive),
            lambda: self.nfs.rm(self._path("nfs", path), recursive=recursive),
            lambda _: None,
        )

    @rule(source=st.sampled_from(_FILES), destination=st.sampled_from(_FILES))
    def copy_file(self, source, destination):
        if source == destination:
            return
        self._compare(
            f"cp_file({source!r}, {destination!r})",
            lambda: self.local.cp_file(
                self._path("local", source), self._path("local", destination)
            ),
            lambda: self.nfs.cp_file(
                self._path("nfs", source), self._path("nfs", destination)
            ),
            lambda _: None,
        )

    @rule(source=st.sampled_from(_FILES), destination=st.sampled_from(_FILES))
    def move_file(self, source, destination):
        if source == destination:
            return
        self._compare(
            f"mv({source!r}, {destination!r})",
            lambda: self.local.mv(
                self._path("local", source), self._path("local", destination)
            ),
            lambda: self.nfs.mv(
                self._path("nfs", source), self._path("nfs", destination)
            ),
            lambda _: None,
        )

    @rule(
        path=st.sampled_from(_FILES + _DIRS),
        start=st.one_of(st.none(), st.integers(-80, 80)),
        end=st.one_of(st.none(), st.integers(-80, 80)),
    )
    def cat_range(self, path, start, end):
        self._compare(
            f"cat_file({path!r}, start={start!r}, end={end!r})",
            lambda: self.local.cat_file(
                self._path("local", path), start=start, end=end
            ),
            lambda: self.nfs.cat_file(self._path("nfs", path), start=start, end=end),
        )

    @rule(
        path=st.sampled_from(_FILES + _DIRS),
        reads=st.lists(
            st.tuples(
                st.integers(-80, 80),
                st.integers(-1, 40),
            ),
            min_size=1,
            max_size=8,
        ),
        cache_type=st.sampled_from(("none", "readahead", "bytes", "blockcache")),
    )
    def buffered_reads(self, path, reads, cache_type):
        def read(fs, backend):
            kwargs = (
                {"cache_type": cache_type, "block_size": 8} if backend == "nfs" else {}
            )
            output = []
            with fs.open(self._path(backend, path), "rb", **kwargs) as handle:
                for offset, length in reads:
                    position = handle.seek(offset, io.SEEK_SET)
                    output.append((position, handle.read(length)))
            return output

        self._compare(
            f"buffered_reads({path!r}, {reads!r}, cache_type={cache_type!r})",
            lambda: read(self.local, "local"),
            lambda: read(self.nfs, "nfs"),
        )

    @rule(directory=st.sampled_from(("/",) + _DIRS), detail=st.booleans())
    def list_directory(self, directory, detail):
        def listing(fs, backend):
            values = fs.ls(self._path(backend, directory), detail=detail)
            if detail:
                return sorted(
                    (self._logical(backend, item["name"]), item["type"], item["size"])
                    for item in values
                )
            return sorted(self._logical(backend, item) for item in values)

        self._compare(
            f"ls({directory!r}, detail={detail!r})",
            lambda: listing(self.local, "local"),
            lambda: listing(self.nfs, "nfs"),
        )


class LocalCacheOracleStateMachine(RuleBasedStateMachine):
    """Check cached nfs4fs results against fresh LocalFileSystem results.

    The nfs4fs side composes its listing, per-open, and persistent
    ``blockcache`` layers. Fresh results are compared with LocalFileSystem;
    paired fsspec wrappers are used only for stable long-lived read handles.
    This avoids treating known LocalFileSystem blockcache-wrapper defects as
    the oracle while retaining its public filesystem behavior as the limit.
    """

    _TTL = 5.0

    def __init__(self):
        super().__init__()
        self._tmp = tempfile.TemporaryDirectory(prefix="nfs4fs-cache-fuzz-")
        self._local_root = os.path.join(self._tmp.name, "local")
        self._dummy_root = os.path.join(self._tmp.name, "dummy")
        self._local_cache_storage = os.path.join(self._tmp.name, "local-cache")
        self._nfs_cache_storage = os.path.join(self._tmp.name, "nfs-cache")
        os.mkdir(self._local_root)

        # A deterministic clock makes listing-cache expiry a generated state
        # transition rather than a wall-clock race.  Patching through the
        # module used by fsspec's DirCache keeps its implementation as oracle.
        self._now = 1_000.0
        self._clock_patch = patch(
            "fsspec.dircache.time.time", side_effect=lambda: self._now
        )
        self._clock_patch.start()

        self.local = fsspec.filesystem("file", skip_instance_cache=True)
        self.local_external = fsspec.filesystem("file", skip_instance_cache=True)
        self.nfs = fsspec.filesystem(
            "nfs4",
            backend="dummy",
            dummy_root=self._dummy_root,
            skip_instance_cache=True,
            use_listings_cache=True,
            listings_expiry_time=self._TTL,
            max_paths=3,
            block_size=8,
        )
        self.nfs_external = fsspec.filesystem(
            "nfs4",
            backend="dummy",
            dummy_root=self._dummy_root,
            skip_instance_cache=True,
            use_listings_cache=False,
            block_size=8,
        )
        self._make_cached_filesystems()
        self._open_handles = None
        self._open_path = None

        # Start from a useful nested tree so reads and cache transitions are
        # exercised even in short, aggressively shrunk histories.
        for directory in ("/d0/sub", "/d1"):
            self.local.makedirs(self._path("local", directory), exist_ok=True)
            self.nfs.makedirs(self._path("nfs", directory), exist_ok=True)
        for path, data in (("/a", b"abcdefgh"), ("/d0/a", b"0123456789")):
            self.local.pipe_file(self._path("local", path), data)
            self.nfs.pipe_file(self._path("nfs", path), data)

    def _make_cached_filesystems(self):
        options = {
            "cache_check": 0,
            "check_files": True,
            "expiry_time": 0,
            "skip_instance_cache": True,
        }
        self.local_cached = fsspec.filesystem(
            "blockcache",
            fs=self.local,
            cache_storage=self._local_cache_storage,
            **options,
        )
        self.nfs_cached = fsspec.filesystem(
            "blockcache",
            fs=self.nfs,
            cache_storage=self._nfs_cache_storage,
            **options,
        )

    def teardown(self):
        if self._open_handles is not None:
            for handle in self._open_handles:
                try:
                    handle.close()
                except Exception:
                    pass
            self._open_handles = None
        for cache in (self.local_cached, self.nfs_cached):
            try:
                cache.clear_cache()
            except Exception:
                pass
        self.nfs_external.close()
        self.nfs.close()
        self._clock_patch.stop()
        self._tmp.cleanup()
        super().teardown()

    def _path(self, backend, logical):
        if backend == "local":
            return self._local_root + logical
        return "nfs4://" + logical

    def _logical(self, backend, path):
        if backend == "local":
            relative = os.path.relpath(path, self._local_root)
            return "/" if relative == "." else "/" + relative.replace(os.sep, "/")
        stripped = self.nfs._strip_protocol(path)
        return "/" if stripped == "/" else "/" + stripped.lstrip("/")

    @staticmethod
    def _capture(call, normalize=lambda value: value):
        try:
            return "ok", normalize(call())
        except Exception as error:
            return "error", _error_signature(error)

    def _compare(self, operation, local_call, nfs_call, normalize=lambda value: value):
        local = self._capture(local_call, normalize)
        nfs = self._capture(nfs_call, normalize)
        assert nfs == local, f"{operation}: local={local!r}, nfs4fs={nfs!r}"
        return local

    @staticmethod
    def _close_local_cached(handle):
        """Close a stock LocalFileSystem blockcache handle.

        fsspec 2026.7.0 completes its cache metadata update and closes the
        underlying LocalFileOpener, then tries to assign to its read-only
        ``closed`` property. Treat that specific upstream post-close error as
        success so the oracle models I/O semantics instead of requiring
        nfs4fs to reproduce an fsspec implementation bug.
        """
        try:
            handle.close()
        except AttributeError as error:
            if "property 'closed'" not in str(error) or not handle.closed:
                raise

    def _snapshot(self, backend):
        fs = self.local_external if backend == "local" else self.nfs_external
        root = self._local_root if backend == "local" else "nfs4:///"
        found = fs.find(root, withdirs=True, detail=True)
        snapshot = {}
        for path, info in found.items():
            logical = self._logical(backend, path)
            if logical == "/":
                continue
            kind = info["type"]
            if kind == "file":
                contents = fs.cat_file(self._path(backend, logical))
                snapshot[logical] = (kind, len(contents), contents)
            else:
                snapshot[logical] = (kind, None, None)
        return snapshot

    @staticmethod
    def _listing(fs, path, backend, logical, detail, refresh=False):
        values = fs.ls(path, detail=detail, refresh=refresh)
        if detail:
            normalized = []
            for item in values:
                name = item["name"]
                if backend == "local":
                    name = "/" + os.path.relpath(name, logical).replace(os.sep, "/")
                else:
                    name = "/" + fs._strip_protocol(name).lstrip("/")
                normalized.append((name, item["type"], item.get("size")))
            return sorted(normalized)
        if backend == "local":
            return sorted(
                "/" + os.path.relpath(name, logical).replace(os.sep, "/")
                for name in values
            )
        return sorted("/" + fs._strip_protocol(name).lstrip("/") for name in values)

    @invariant()
    def backing_trees_match(self):
        assert self._snapshot("nfs") == self._snapshot("local")

    @precondition(lambda self: self._open_handles is None)
    @rule(
        path=st.sampled_from(_FILES),
        data=_BLOBS,
        external=st.booleans(),
    )
    def write_through_target(self, path, data, external):
        local = self.local_external if external else self.local
        nfs = self.nfs_external if external else self.nfs
        self._compare(
            f"target.pipe_file({path!r}, external={external!r})",
            lambda: local.pipe_file(self._path("local", path), data),
            lambda: nfs.pipe_file(self._path("nfs", path), data),
            lambda _: None,
        )
        if external:
            # External mutation is observable through a LocalFileSystem
            # listing immediately.  nfs4fs promises the equivalent after
            # the configured fsspec TTL, so cross that public boundary.
            self._now += self._TTL + 0.1

    @precondition(lambda self: self._open_handles is None)
    @rule(
        path=st.sampled_from(_FILES),
        data=_BLOBS,
        mode=st.sampled_from(("wb", "ab", "xb")),
    )
    def write_through_persistent_cache(self, path, data, mode):
        def write(fs, backend):
            with fs.open(self._path(backend, path), mode) as handle:
                return handle.write(data)

        self._compare(
            f"blockcache.open({path!r}, {mode!r})",
            lambda: write(self.local, "local"),
            lambda: write(self.nfs_cached, "nfs"),
        )

    @rule(
        path=st.sampled_from(_FILES + _DIRS),
        offset=st.integers(-24, 40),
        length=st.integers(-1, 32),
        whence=st.sampled_from((io.SEEK_SET, io.SEEK_CUR, io.SEEK_END)),
        block_size=st.sampled_from((1, 2, 4, 8, 16)),
    )
    def persistent_cached_read(self, path, offset, length, whence, block_size):
        def read(fs, backend):
            handle = fs.open(self._path(backend, path), "rb", block_size=block_size)
            try:
                position = handle.seek(offset, whence)
                return position, handle.read(length)
            finally:
                handle.close()

        self._compare(
            f"blockcache.read({path!r}, {offset!r}, {length!r}, {whence!r})",
            lambda: read(self.local, "local"),
            lambda: read(self.nfs_cached, "nfs"),
        )

    @rule(
        path=st.sampled_from(_FILES + _DIRS),
        offset=st.integers(-24, 40),
        buffer_size=st.integers(0, 32),
        whence=st.sampled_from((io.SEEK_SET, io.SEEK_CUR, io.SEEK_END)),
        block_size=st.sampled_from((1, 2, 4, 8, 16)),
    )
    def persistent_cached_readinto(self, path, offset, buffer_size, whence, block_size):
        def readinto(fs, backend):
            target = bytearray(buffer_size)
            with fs.open(
                self._path(backend, path), "rb", block_size=block_size
            ) as handle:
                position = handle.seek(offset, whence)
                count = handle.readinto(target)
                return position, count, bytes(target)

        self._compare(
            f"blockcache.readinto({path!r}, {offset!r}, {buffer_size!r}, {whence!r})",
            lambda: readinto(self.local, "local"),
            lambda: readinto(self.nfs_cached, "nfs"),
        )

    @rule(
        paths=st.lists(st.sampled_from(_FILES), min_size=1, max_size=4, unique=True),
        offset=st.integers(0, 24),
        length=st.integers(-1, 24),
    )
    def persistent_open_many_read(self, paths, offset, length):
        def local_read():
            output = []
            for path in paths:
                with self.local.open(self._path("local", path), "rb") as handle:
                    handle.seek(offset)
                    output.append(handle.read(length))
            return output

        def nfs_read():
            entries = [
                OpenFile(
                    self.nfs_cached,
                    self._path("nfs", path),
                    mode="rb",
                )
                for path in paths
            ]
            with OpenFiles(entries, mode="rb", fs=self.nfs_cached) as handles:
                for handle in handles:
                    handle.seek(offset)
                return [handle.read(length) for handle in handles]

        self._compare(
            f"blockcache.open_many({paths!r}, {offset!r}, {length!r})",
            local_read,
            nfs_read,
        )

    @precondition(lambda self: self._open_handles is None)
    @rule(path=st.sampled_from(_FILES), block_size=st.sampled_from((1, 4, 8, 16)))
    def open_long_lived_cached_reader(self, path, block_size):
        # Start both fsspec wrappers from the current backing generation. This
        # isolates active-handle caching semantics from known stale-generation
        # defects in stock LocalFileSystem's persistent wrapper.
        for cache, backend in (
            (self.local_cached, "local"),
            (self.nfs_cached, "nfs"),
        ):
            try:
                cache_path = self._path(backend, path) if backend == "local" else path
                cache.pop_from_cache(cache_path)
            except FileNotFoundError:
                pass
        local = self._capture(
            lambda: self.local_cached.open(
                self._path("local", path), "rb", block_size=block_size
            )
        )
        effective_block_size = local[1].blocksize if local[0] == "ok" else block_size
        nfs = self._capture(
            lambda: self.nfs_cached.open(
                self._path("nfs", path), "rb", block_size=effective_block_size
            )
        )
        if local[0] == "error" or nfs[0] == "error":
            if local[0] == "ok":
                self._close_local_cached(local[1])
            if nfs[0] == "ok":
                nfs[1].close()
            assert (nfs[0], nfs[1]) == (
                local[0],
                local[1],
            ), f"blockcache.open({path!r}): local={local!r}, nfs4fs={nfs!r}"
            return
        self._open_handles = (local[1], nfs[1])
        self._open_path = path

    @precondition(lambda self: self._open_handles is not None)
    @rule(
        offset=st.integers(-24, 40),
        length=st.integers(-1, 32),
        whence=st.sampled_from((io.SEEK_SET, io.SEEK_CUR, io.SEEK_END)),
    )
    def read_long_lived_cached_reader(self, offset, length, whence):
        local_handle, nfs_handle = self._open_handles
        self._compare(
            f"live.read({self._open_path!r}, {offset!r}, {length!r}, {whence!r})",
            lambda: (local_handle.seek(offset, whence), local_handle.read(length)),
            lambda: (nfs_handle.seek(offset, whence), nfs_handle.read(length)),
        )

    @precondition(lambda self: self._open_handles is not None)
    @rule()
    def close_long_lived_cached_reader(self):
        local_handle, nfs_handle = self._open_handles
        self._open_handles = None
        path = self._open_path
        self._open_path = None
        self._compare(
            f"live.close({path!r})",
            lambda: self._close_local_cached(local_handle),
            nfs_handle.close,
            lambda _: None,
        )

    @rule(
        directory=st.sampled_from(("/",) + _DIRS),
        detail=st.booleans(),
        refresh=st.booleans(),
        repeat=st.booleans(),
    )
    def cached_listing(self, directory, detail, refresh, repeat):
        local_path = self._path("local", directory)
        nfs_path = self._path("nfs", directory)

        def local_listing():
            return self._listing(
                self.local, local_path, "local", self._local_root, detail, refresh
            )

        def nfs_listing():
            return self._listing(
                self.nfs, nfs_path, "nfs", self._dummy_root, detail, refresh
            )

        self._compare(
            f"ls({directory!r}, refresh={refresh!r})", local_listing, nfs_listing
        )
        if repeat:
            self._compare(
                f"ls({directory!r}, cached repeat)", local_listing, nfs_listing
            )

    @rule(amount=st.sampled_from((0.0, 2.5, 5.1, 11.0)))
    def advance_cache_clock(self, amount):
        self._now += amount

    @rule(path=st.one_of(st.none(), st.sampled_from(("/",) + _DIRS + _FILES)))
    def invalidate_listing(self, path):
        self._compare(
            f"invalidate_cache({path!r})",
            lambda: self.local.invalidate_cache(
                None if path is None else self._path("local", path)
            ),
            lambda: self.nfs.invalidate_cache(
                None if path is None else self._path("nfs", path)
            ),
            lambda _: None,
        )

    @precondition(lambda self: self._open_handles is None)
    @rule(directory=st.sampled_from(_DIRS), exist_ok=st.booleans())
    def make_directory_through_persistent_cache(self, directory, exist_ok):
        self._compare(
            f"blockcache.makedirs({directory!r}, exist_ok={exist_ok!r})",
            lambda: self.local.makedirs(
                self._path("local", directory), exist_ok=exist_ok
            ),
            lambda: self.nfs_cached.makedirs(
                self._path("nfs", directory), exist_ok=exist_ok
            ),
            lambda _: None,
        )

    @precondition(lambda self: self._open_handles is None)
    @rule(source=st.sampled_from(_FILES), destination=st.sampled_from(_FILES))
    def copy_through_persistent_cache(self, source, destination):
        if source == destination:
            return
        self._compare(
            f"blockcache.cp_file({source!r}, {destination!r})",
            lambda: self.local.cp_file(
                self._path("local", source), self._path("local", destination)
            ),
            lambda: self.nfs_cached.cp_file(
                self._path("nfs", source), self._path("nfs", destination)
            ),
            lambda _: None,
        )

    @precondition(lambda self: self._open_handles is None)
    @rule()
    def clear_persistent_caches(self):
        self.local_cached.clear_cache()
        self.nfs_cached.clear_cache()

    @precondition(lambda self: self._open_handles is None)
    @rule(source=st.sampled_from(_FILES), destination=st.sampled_from(_FILES))
    def move_through_persistent_cache(self, source, destination):
        if source == destination:
            return
        self._compare(
            f"blockcache.mv({source!r}, {destination!r})",
            lambda: self.local.mv(
                self._path("local", source), self._path("local", destination)
            ),
            lambda: self.nfs_cached.mv(
                self._path("nfs", source), self._path("nfs", destination)
            ),
            lambda _: None,
        )

    @precondition(lambda self: self._open_handles is None)
    @rule(path=st.sampled_from(_FILES + _DIRS), recursive=st.booleans())
    def remove_through_persistent_cache(self, path, recursive):
        self._compare(
            f"blockcache.rm({path!r}, recursive={recursive!r})",
            lambda: self.local.rm(self._path("local", path), recursive=recursive),
            lambda: self.nfs_cached.rm(self._path("nfs", path), recursive=recursive),
            lambda _: None,
        )

    @precondition(lambda self: self._open_handles is None)
    @rule(path=st.sampled_from(_FILES))
    def evict_persistent_file(self, path):
        for cache, backend in (
            (self.local_cached, "local"),
            (self.nfs_cached, "nfs"),
        ):
            try:
                cache_path = self._path(backend, path) if backend == "local" else path
                cache.pop_from_cache(cache_path)
            except FileNotFoundError:
                pass

    @precondition(lambda self: self._open_handles is None)
    @rule()
    def reconstruct_persistent_cache(self):
        # Reopening the wrapper over the same storage exercises persisted
        # metadata generations without asserting anything beyond stock
        # LocalFileSystem/blockcache behavior.
        self._make_cached_filesystems()


TestLocalOracle = LocalOracleStateMachine.TestCase
TestLocalOracle.settings = settings(
    max_examples=_FUZZ_EXAMPLES,
    stateful_step_count=_FUZZ_STEPS,
    deadline=None,
    derandomize=True,
    suppress_health_check=(HealthCheck.too_slow,),
)

TestLocalCacheOracle = LocalCacheOracleStateMachine.TestCase
TestLocalCacheOracle.settings = settings(
    max_examples=_FUZZ_EXAMPLES,
    stateful_step_count=_FUZZ_STEPS,
    deadline=None,
    derandomize=True,
    suppress_health_check=(HealthCheck.too_slow,),
)
