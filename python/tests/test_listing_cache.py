"""fsspec-compatible TTL directory listing cache tests."""

import pickle
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from types import SimpleNamespace

import fsspec
import fsspec.dircache
import pytest


def _cached_fs(tmp_path, **kwargs):
    return fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        skip_instance_cache=True,
        use_listings_cache=True,
        **kwargs,
    )


@pytest.fixture
def cached_fs(tmp_path):
    fs = _cached_fs(tmp_path)
    yield fs
    fs.close()


def _count_calls(obj, name):
    calls = []
    original = getattr(obj, name)

    def recording(*args, **kwargs):
        calls.append((args, kwargs))
        return original(*args, **kwargs)

    setattr(obj, name, recording)
    return calls


def test_listing_cache_is_disabled_by_default(tmp_path):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        skip_instance_cache=True,
    )
    fs.pipe_file("/file", b"data")
    calls = _count_calls(fs._client, "listdir")

    fs.ls("/")
    fs.ls("/")

    assert len(calls) == 2
    assert not fs.dircache.use_listings_cache
    assert len(fs.dircache) == 0
    fs.close()


def test_ls_caches_details_honors_refresh_and_returns_defensive_copies(tmp_path):
    fs = _cached_fs(tmp_path)
    fs.pipe_file("/file", b"data")
    calls = _count_calls(fs._client, "listdir")

    first = fs.ls("/", detail=True)
    first[0]["size"] = 999
    first.append({"name": "corrupt", "type": "file", "size": 0})
    second = fs.ls("/", detail=True)

    assert len(calls) == 1
    assert second[0]["size"] == 4
    assert len(second) == 1
    assert fs.ls("/", detail=False) == ["nfs4:///file"]
    assert len(calls) == 1
    fs.ls("/", refresh=True)
    assert len(calls) == 2
    fs.close()


def test_concurrent_mutation_cannot_be_overwritten_by_an_older_listing(
    cached_fs, monkeypatch
):
    fs = cached_fs
    native_listing_ready = threading.Event()
    allow_listing_to_return = threading.Event()
    original_listdir = fs._client.listdir

    def pause_after_native_listing(path):
        listing = original_listdir(path)
        native_listing_ready.set()
        assert allow_listing_to_return.wait(timeout=5)
        return listing

    monkeypatch.setattr(fs._client, "listdir", pause_after_native_listing)
    with ThreadPoolExecutor(max_workers=1) as pool:
        listing = pool.submit(fs.ls, "/", False)
        try:
            assert native_listing_ready.wait(timeout=5)
            fs.pipe_file("/created", b"data")
        finally:
            allow_listing_to_return.set()
        assert listing.result(timeout=5) == []

    assert fs.ls("/", detail=False) == ["nfs4:///created"]


def test_listing_expiry_time_uses_fsspec_ttl(tmp_path, monkeypatch):
    now = [100.0]
    monkeypatch.setattr(fsspec.dircache.time, "time", lambda: now[0])
    fs = _cached_fs(tmp_path, listings_expiry_time=5.0)
    fs.pipe_file("/file", b"data")
    calls = _count_calls(fs._client, "listdir")

    fs.ls("/")
    now[0] = 104.9
    fs.ls("/")
    assert len(calls) == 1

    now[0] = 105.1
    fs.ls("/")
    assert len(calls) == 2
    fs.close()


def test_max_paths_is_forwarded_to_fsspec_dircache(tmp_path):
    fs = _cached_fs(tmp_path, max_paths=1)
    for name in ("a", "b", "c"):
        fs.mkdir(f"/{name}")
        fs.pipe_file(f"/{name}/file", name.encode())
    calls = _count_calls(fs._client, "listdir")

    fs.ls("/a")
    fs.ls("/b")
    fs.ls("/c")
    fs.ls("/a")

    assert fs.dircache.max_paths == 1
    assert len(calls) == 4
    fs.close()


def test_walk_populates_and_reuses_a_complete_cached_subtree(tmp_path):
    fs = _cached_fs(tmp_path)
    fs.makedirs("/tree/a", exist_ok=True)
    fs.makedirs("/tree/b", exist_ok=True)
    fs.pipe({"/tree/top": b"0", "/tree/a/x": b"1", "/tree/b/y": b"22"})
    calls = _count_calls(fs._client, "walk")

    first = list(fs.walk("/tree"))
    assert len(calls) == 1
    assert set(fs.dircache) == {"/tree", "/tree/a", "/tree/b"}

    assert list(fs.walk("/tree")) == first
    assert fs.find("/tree") == ["/tree/a/x", "/tree/b/y", "/tree/top"]
    assert fs.glob("/tree/*/*") == ["/tree/a/x", "/tree/b/y"]
    assert fs.du("/tree") == 4
    assert len(calls) == 1

    list(fs.walk("/tree", refresh=True))
    assert len(calls) == 2
    fs.close()


def test_expired_descendant_refreshes_the_vectorized_subtree(tmp_path, monkeypatch):
    now = [100.0]
    monkeypatch.setattr(fsspec.dircache.time, "time", lambda: now[0])
    fs = _cached_fs(tmp_path, listings_expiry_time=5.0)
    fs.makedirs("/tree/child", exist_ok=True)
    fs.pipe_file("/tree/child/file", b"data")
    calls = _count_calls(fs._client, "walk")

    list(fs.walk("/tree"))
    assert len(calls) == 1
    now[0] = 106.0
    list(fs.walk("/tree"))

    assert len(calls) == 2
    assert set(fs.dircache) == {"/tree", "/tree/child"}
    fs.close()


def test_namespace_mutations_invalidate_affected_listings(tmp_path):
    fs = _cached_fs(tmp_path)
    fs.makedirs("/source", exist_ok=True)
    fs.makedirs("/dest", exist_ok=True)
    fs.pipe_file("/source/file", b"one")

    fs.ls("/source")
    fs.pipe_file("/source/file", b"longer")
    assert fs.ls("/source")[0]["size"] == 6

    fs.ls("/")
    fs.mkdir("/new-dir")
    assert "nfs4:///new-dir" in fs.ls("/", detail=False)
    fs.rmdir("/new-dir")
    assert "nfs4:///new-dir" not in fs.ls("/", detail=False)

    fs.ls("/source")
    fs.ls("/dest")
    fs.mv("/source/file", "/dest/moved")
    assert fs.ls("/source") == []
    assert fs.ls("/dest", detail=False) == ["nfs4:///dest/moved"]

    fs.copy("/dest/moved", "/source/copied")
    assert fs.ls("/source", detail=False) == ["nfs4:///source/copied"]
    fs.rm("/source/copied")
    assert fs.ls("/source") == []
    fs.close()


def test_child_creation_invalidates_cached_parent_directory_metadata(cached_fs):
    fs = cached_fs
    fs.mkdir("/parent")
    original_nlink = fs.ls("/")[0]["nlink"]

    fs.mkdir("/parent/child")

    parent = fs.ls("/")[0]
    assert parent["nlink"] == original_nlink + 1


def test_open_file_writes_and_truncates_invalidate_recached_details(tmp_path):
    fs = _cached_fs(tmp_path)
    fs.pipe_file("/file", b"original")

    with fs.open("/file", "wb") as file:
        assert fs.ls("/")[0]["size"] == 0
        file.write(b"replacement")
        assert fs.ls("/")[0]["size"] == 11

    with fs.open("/file", "rb+") as file:
        fs.ls("/")
        file.truncate(3)
        assert fs.ls("/")[0]["size"] == 3
    fs.close()


def test_failed_mutation_still_invalidates_possibly_stale_listing(
    tmp_path, monkeypatch
):
    fs = _cached_fs(tmp_path)
    fs.pipe_file("/file", b"old")
    fs.ls("/")
    assert "/" in fs.dircache

    def fail_write(*args, **kwargs):
        raise ConnectionError("ambiguous write")

    monkeypatch.setattr(fs._client, "write_many", fail_write)
    with pytest.raises(ConnectionError, match="ambiguous write"):
        fs.pipe_file("/file", b"new")

    assert "/" not in fs.dircache
    fs.close()


def test_link_creation_invalidates_parent_listing(tmp_path):
    fs = _cached_fs(tmp_path)
    fs.pipe_file("/target", b"data")
    fs.ls("/")
    fs.symlink("target", "/sym")
    assert "nfs4:///sym" in fs.ls("/", detail=False)

    fs.ls("/")
    fs.hardlink("/target", "/hard")
    assert "nfs4:///hard" in fs.ls("/", detail=False)
    fs.close()


def test_hardlink_creation_invalidates_cached_source_metadata(cached_fs):
    fs = cached_fs
    fs.makedirs("/source", exist_ok=True)
    fs.makedirs("/dest", exist_ok=True)
    fs.pipe_file("/source/file", b"data")
    assert fs.ls("/source")[0]["nlink"] == 1

    fs.hardlink("/source/file", "/dest/link")

    assert fs.ls("/source")[0]["nlink"] == 2


def test_partial_writable_open_many_failure_invalidates_created_prefix(cached_fs):
    fs = cached_fs
    assert fs.ls("/") == []
    open_files = [
        SimpleNamespace(path="/created", mode="xb"),
        SimpleNamespace(path="/missing/fails", mode="xb"),
    ]

    with pytest.raises(FileNotFoundError):
        fs.open_many(open_files)

    assert fs.exists("/created")
    assert fs.ls("/", detail=False) == ["nfs4:///created"]


def test_manual_invalidation_removes_path_and_descendants(tmp_path):
    fs = _cached_fs(tmp_path)
    fs.makedirs("/tree/child/grandchild", exist_ok=True)
    list(fs.walk("/tree"))
    assert set(fs.dircache) == {
        "/tree",
        "/tree/child",
        "/tree/child/grandchild",
    }

    fs.invalidate_cache("nfs4:///tree/child")

    assert set(fs.dircache) == {"/tree"}
    fs.invalidate_cache()
    assert len(fs.dircache) == 0
    fs.close()


def test_external_mutation_is_visible_after_refresh(tmp_path):
    first = _cached_fs(tmp_path)
    second = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        skip_instance_cache=True,
    )
    first.pipe_file("/old", b"data")
    assert first.ls("/", detail=False) == ["nfs4:///old"]

    second.pipe_file("/new", b"data")
    assert first.ls("/", detail=False) == ["nfs4:///old"]
    assert first.ls("/", detail=False, refresh=True) == [
        "nfs4:///new",
        "nfs4:///old",
    ]
    first.close()
    second.close()


def test_transaction_commit_invalidates_listing(tmp_path):
    fs = _cached_fs(tmp_path)
    assert fs.ls("/") == []

    with fs.transaction:
        with fs.open("/committed", "wb") as file:
            file.write(b"data")
        assert fs.ls("/") == []

    assert fs.ls("/", detail=False) == ["nfs4:///committed"]
    fs.close()


def test_reconnect_and_close_cannot_serve_old_cache_entries(tmp_path):
    fs = _cached_fs(tmp_path)
    fs.pipe_file("/file", b"data")
    calls = _count_calls(fs._client, "listdir")
    fs.ls("/")
    assert len(calls) == 1

    fs._client.reconnect()
    fs.ls("/")
    assert len(calls) == 2

    fs.close()
    with pytest.raises(ValueError, match="closed"):
        fs.ls("/")


def test_cache_configuration_serializes_without_cached_contents(tmp_path):
    fs = _cached_fs(tmp_path, listings_expiry_time=3.0, max_paths=7)
    fs.pipe_file("/file", b"data")
    fs.ls("/")
    assert len(fs.dircache) == 1

    restored = pickle.loads(pickle.dumps(fs))

    assert restored.dircache.use_listings_cache
    assert restored.dircache.listings_expiry_time == 3.0
    assert restored.dircache.max_paths == 7
    assert len(restored.dircache) == 0
    restored.close()
    fs.close()


def test_cached_walk_handles_trees_deeper_than_python_recursion_limit(cached_fs):
    fs = cached_fs
    directory = "/"
    levels = sys.getrecursionlimit() + 50
    for _ in range(levels):
        child = directory.rstrip("/") + "/d"
        fs.dircache[directory] = [{"name": fs._fullpath(child), "type": "directory"}]
        directory = child
    fs.dircache[directory] = []

    try:
        walked = list(fs.walk("/"))
    except RecursionError:
        pytest.fail("cached walk exceeded Python's recursion limit", pytrace=False)
    assert len(walked) == levels + 1


def test_bulk_namespace_invalidation_scans_dircache_at_most_once(cached_fs):
    class CountingPath(str):
        prefix_checks = 0

        def startswith(self, *args, **kwargs):
            type(self).prefix_checks += 1
            return super().startswith(*args, **kwargs)

    class CountingDirCache(fsspec.dircache.DirCache):
        def __init__(self):
            super().__init__(use_listings_cache=True)
            self.scans = 0

        def __iter__(self):
            self.scans += 1
            return super().__iter__()

    fs = cached_fs
    fs.dircache = CountingDirCache()
    for index in range(32):
        fs.dircache[CountingPath(f"/cached-{index}")] = []

    fs._invalidate_namespace(f"/target/file-{index}" for index in range(32))

    assert fs.dircache.scans <= 1
    assert CountingPath.prefix_checks <= 32


@pytest.mark.parametrize("fs_fixture", ["dummy_fs", "nfs_fs", "smb_fs"])
def test_listing_cache_smoke_on_every_backend(request, fs_fixture):
    fs = request.getfixturevalue(fs_fixture)
    fs.dircache.use_listings_cache = True
    calls = _count_calls(fs._client, "listdir")

    fs.mkdir("/cache-smoke")
    fs.pipe_file("/cache-smoke/a", b"data")
    assert fs.ls("/cache-smoke", detail=False) == ["nfs4:///cache-smoke/a"]
    assert fs.ls("/cache-smoke", detail=False) == ["nfs4:///cache-smoke/a"]
    assert len(calls) == 1

    fs.pipe_file("/cache-smoke/b", b"more")
    assert fs.ls("/cache-smoke", detail=False) == [
        "nfs4:///cache-smoke/a",
        "nfs4:///cache-smoke/b",
    ]
    assert len(calls) == 2
