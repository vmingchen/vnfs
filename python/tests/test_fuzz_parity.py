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

import fsspec
from hypothesis import HealthCheck, settings
from hypothesis import strategies as st
from hypothesis.stateful import RuleBasedStateMachine, invariant, rule

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
        # nfs4fs currently treats mkdir(existing, create_parents=True) as the
        # idempotent root/bootstrap operation used by its integration fixtures.
        # Existing-directory semantics are covered by the makedirs rule.
        if self.local.exists(self._path("local", directory)):
            return
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
        # VFSI currently has no timestamp-only mutation, so nfs4fs documents
        # touch(existing, truncate=False) as unsupported.
        if not truncate and self.local.exists(self._path("local", path)):
            return
        self._compare(
            f"touch({path!r}, truncate={truncate!r})",
            lambda: self.local.touch(self._path("local", path), truncate=truncate),
            lambda: self.nfs.touch(self._path("nfs", path), truncate=truncate),
            lambda _: None,
        )

    @rule(path=st.sampled_from(_FILES + _DIRS), recursive=st.booleans())
    def remove(self, path, recursive):
        # LocalFileSystem reports ValueError for rm(non-empty directory,
        # recursive=False), while nfs4fs intentionally follows os.remove and
        # reports IsADirectoryError. Directory removal itself is fuzzed via
        # recursive rm; non-recursive rm remains covered for files.
        if not recursive and self.local.isdir(self._path("local", path)):
            return
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
        if not self.local.isfile(self._path("local", path)):
            return
        size = self.local.size(self._path("local", path))
        normalized_start = 0 if start is None else start
        if normalized_start < 0:
            normalized_start = max(0, size + normalized_start)
        normalized_end = end
        if normalized_end is not None and normalized_end < 0:
            normalized_end = size + normalized_end
        # LocalFileSystem can turn an invalid negative read length into an
        # unbounded read. nfs4fs intentionally validates this fsspec range
        # contract, so use local as the oracle only for valid slice bounds.
        if normalized_end is not None and normalized_end < normalized_start:
            return
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
            # A zero-byte read does not force nfs4fs's deliberately lazy read
            # descriptor open, unlike LocalFileSystem's eager OS open.
            st.tuples(
                st.integers(0, 80),
                st.one_of(st.just(-1), st.integers(1, 40)),
            ),
            min_size=1,
            max_size=8,
        ),
        cache_type=st.sampled_from(("none", "readahead", "bytes", "blockcache")),
    )
    def buffered_reads(self, path, reads, cache_type):
        # LocalFileSystem opens eagerly while nfs4fs defers read OPEN so that
        # OpenFiles can batch descriptors. Missing paths are compared through
        # cat_file; handle behavior is compared only once a file exists.
        if not self.local.isfile(self._path("local", path)):
            return

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


TestLocalOracle = LocalOracleStateMachine.TestCase
TestLocalOracle.settings = settings(
    max_examples=_FUZZ_EXAMPLES,
    stateful_step_count=_FUZZ_STEPS,
    deadline=None,
    derandomize=True,
    suppress_health_check=(HealthCheck.too_slow,),
)
