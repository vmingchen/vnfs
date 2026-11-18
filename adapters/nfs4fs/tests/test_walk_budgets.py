"""Whole-walk limits and one-fetch-per-directory regressions."""

import errno
from collections import Counter

import fsspec
import pytest


@pytest.mark.parametrize("cached", [False, True])
@pytest.mark.parametrize("limit", ["entries", "paths"])
@pytest.mark.parametrize("operation", ["topdown", "bottomup", "fallback", "find"])
def test_walk_shares_limits_across_directories(
    tmp_path, monkeypatch, cached, limit, operation
):
    options = (
        {"directory_max_entries": 3}
        if limit == "entries"
        else {"directory_max_path_bytes": 10}
    )
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        use_listings_cache=cached,
        skip_instance_cache=True,
        **options,
    )
    try:
        for name in ("a", "b"):
            fs.mkdir(f"/{name}")
            fs.pipe({f"/{name}/x": b"x", f"/{name}/y": b"y"})
        if cached:
            for directory in ("/", "/a", "/b"):
                fs.ls(directory)
        if operation == "fallback":

            def fail_walk(*args, **kwargs):
                raise PermissionError("injected recursive listing failure")

            monkeypatch.setattr(fs._client, "walk", fail_walk)
        with pytest.raises(OSError) as error:
            if operation == "find":
                fs.find("/")
            else:
                list(
                    fs.walk(
                        "/",
                        topdown=operation == "topdown",
                        refresh=operation == "fallback",
                    )
                )
        assert error.value.errno == errno.EFBIG
    finally:
        fs.close()


@pytest.mark.parametrize("cached", [False, True])
def test_walk_fetches_each_selected_directory_once(tmp_path, monkeypatch, cached):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        use_listings_cache=cached,
        batch_size=2,
        skip_instance_cache=True,
    )
    try:
        for name in ("a", "b", "c"):
            fs.mkdir(f"/{name}")
            fs.pipe_file(f"/{name}/x", b"x")
        calls = Counter()
        original = fs._client.listdir
        original_many = fs._client.listdir_many

        def listdir(path, **kwargs):
            calls[path] += 1
            return original(path, **kwargs)

        def listdir_many(paths, **kwargs):
            calls.update(paths)
            assert len(paths) <= fs.batch_size
            return original_many(paths, **kwargs)

        monkeypatch.setattr(fs._client, "listdir", listdir)
        monkeypatch.setattr(fs._client, "listdir_many", listdir_many)
        assert [directory for directory, _, _ in fs.walk("/")] == [
            "/",
            "/a",
            "/b",
            "/c",
        ]
        assert calls == Counter({"/a": 1, "/b": 1, "/c": 1})
    finally:
        fs.close()


@pytest.mark.parametrize("cached", [False, True])
@pytest.mark.parametrize("topdown", [False, True])
def test_walk_accepts_exact_aggregate_budget(tmp_path, cached, topdown):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        directory_max_entries=6,
        directory_max_path_bytes=20,
        use_listings_cache=cached,
        skip_instance_cache=True,
    )
    try:
        for name in ("a", "b"):
            fs.mkdir(f"/{name}")
            fs.pipe({f"/{name}/x": b"x", f"/{name}/y": b"y"})
        for _ in range(2):
            assert len(list(fs.walk("/", topdown=topdown))) == 3
    finally:
        fs.close()


def test_walk_passes_remaining_budget_to_native_fetch(tmp_path, monkeypatch):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        directory_max_entries=3,
        directory_max_path_bytes=20,
        skip_instance_cache=True,
    )
    try:
        for name in ("a", "b"):
            fs.mkdir(f"/{name}")
            fs.pipe({f"/{name}/x": b"x", f"/{name}/y": b"y"})
        original = fs._client.listdir_many
        calls = []

        def listdir_many(paths, **kwargs):
            calls.append(kwargs)
            return original(paths, **kwargs)

        monkeypatch.setattr(fs._client, "listdir_many", listdir_many)
        with pytest.raises(OSError) as error:
            list(fs.walk("/"))
        assert error.value.errno == errno.EFBIG
        assert calls == [{"recursive": False, "max_entries": 1, "max_path_bytes": 16}]
        # Strict on_error disables prefetch, but scalar fetches still share the limit.
        with pytest.raises(OSError) as error:
            list(fs.walk("/", on_error="raise"))
        assert error.value.errno == errno.EFBIG
    finally:
        fs.close()


def test_native_directory_overrides_are_bounded_and_zero_is_an_empty_budget(dummy_fs):
    dummy_fs.mkdir("/a")
    dummy_fs.mkdir("/b")
    assert dummy_fs._client.listdir("/a", max_entries=0, max_path_bytes=0) == []
    dummy_fs.pipe_file("/a/x", b"x")
    dummy_fs.pipe_file("/b/y", b"y")
    with pytest.raises(OSError) as error:
        dummy_fs._client.listdir("/a", max_entries=0)
    assert error.value.errno == errno.EFBIG
    with pytest.raises(OSError) as error:
        dummy_fs._client.listdir_many(["/a", "/b"], max_entries=1)
    assert error.value.errno == errno.EFBIG
    with pytest.raises(OSError) as error:
        dummy_fs._client.listdir_many(["/a", "/b"], max_path_bytes=4)
    assert error.value.errno == errno.EFBIG


@pytest.mark.parametrize("event", ["reconnect", "close"])
def test_walk_prefetch_obeys_client_generation_and_close(tmp_path, event):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        directory_max_entries=6,
        directory_max_path_bytes=20,
        skip_instance_cache=True,
    )
    try:
        for name in ("a", "b"):
            fs.mkdir(f"/{name}")
            fs.pipe({f"/{name}/x": b"x", f"/{name}/y": b"y"})
        iterator = fs.walk("/")
        assert next(iterator)[0] == "/"
        assert next(iterator)[0] == "/a"  # /b is already prefetched and charged.
        if event == "close":
            fs.close()
            with pytest.raises(ValueError, match="filesystem is closed"):
                next(iterator)
        else:
            (tmp_path / "b" / "y").rename(tmp_path / "b" / "z")
            fs._client.reconnect()
            assert next(iterator) == ("/b", [], ["x", "z"])
            assert list(iterator) == []
    finally:
        fs.close()


@pytest.mark.parametrize("cached", [False, True])
@pytest.mark.parametrize(
    "action", ["create", "rename", "remove", "invalidate", "refresh"]
)
def test_walk_prefetch_obeys_namespace_invalidation(tmp_path, cached, action):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        directory_max_entries=6,
        directory_max_path_bytes=20,
        use_listings_cache=cached,
        skip_instance_cache=True,
    )
    try:
        fs.mkdir("/a")
        fs.mkdir("/b")
        fs.pipe({"/a/x": b"x", "/b/x": b"x", "/b/y": b"y"})
        iterator = fs.walk("/")
        assert next(iterator)[0] == "/"
        assert next(iterator)[0] == "/a"  # /b is now prefetched and charged.
        expected = ["x", "z"]
        if action == "create":
            fs.pipe_file("/b/z", b"z")
            expected = ["x", "y", "z"]
        elif action == "rename":
            fs.mv("/b/y", "/b/z")
        elif action == "remove":
            fs.rm("/b/y")
            expected = ["x"]
        else:
            (tmp_path / "b" / "y").rename(tmp_path / "b" / "z")
            if action == "invalidate":
                fs.invalidate_cache("/b")
            else:
                fs.ls("/b", refresh=True)
        assert next(iterator) == ("/b", [], expected)
        assert list(iterator) == []
    finally:
        fs.close()


@pytest.mark.parametrize("cached", [False, True])
@pytest.mark.parametrize("stale_limit_error", [False, True])
def test_walk_discards_prefetch_invalidated_while_fetching(
    tmp_path, monkeypatch, cached, stale_limit_error
):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        directory_max_entries=5,
        directory_max_path_bytes=16,
        use_listings_cache=cached,
        skip_instance_cache=True,
    )
    try:
        fs.mkdir("/a")
        fs.mkdir("/b")
        fs.pipe({"/a/x": b"x", "/b/x": b"x", "/b/y": b"y"})
        original = fs._client.listdir_many

        def invalidated_fetch(paths, **kwargs):
            groups = original(paths, **kwargs)
            fs.mv("/b/y", "/b/z")
            if stale_limit_error:
                raise OSError(errno.EFBIG, "limit from invalidated fetch")
            return groups

        monkeypatch.setattr(fs._client, "listdir_many", invalidated_fetch)
        assert list(fs.walk("/")) == [
            ("/", ["a", "b"], []),
            ("/a", [], ["x"]),
            ("/b", [], ["x", "z"]),
        ]
    finally:
        fs.close()


@pytest.mark.parametrize("cached", [False, True])
@pytest.mark.parametrize("operation", ["topdown", "bottomup", "find"])
@pytest.mark.parametrize("maxdepth", [None, 3, 2])
def test_walk_depth_limit_is_independent_of_cached_listings(
    tmp_path, cached, operation, maxdepth
):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        use_listings_cache=cached,
        walk_max_depth=1,
        skip_instance_cache=True,
    )
    try:
        fs.mkdir("/one/two", create_parents=True)
        fs.pipe_file("/one/two/file", b"x")
        if cached:
            for directory in ("/", "/one", "/one/two"):
                fs.ls(directory)

        def traverse():
            if operation == "find":
                return fs.find("/", maxdepth=maxdepth)
            return list(fs.walk("/", topdown=operation == "topdown", maxdepth=maxdepth))

        if maxdepth == 2:
            # A caller's explicit shallower traversal is allowed to truncate.
            result = traverse()
            if operation == "find":
                assert result == []
            else:
                assert {directory for directory, _, _ in result} == {"/", "/one"}
        else:
            with pytest.raises(OSError) as error:
                traverse()
            assert error.value.errno == errno.EFBIG
    finally:
        fs.close()


def test_cached_walk_allows_pruning_at_the_depth_limit(tmp_path):
    fs = fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        use_listings_cache=True,
        walk_max_depth=1,
        skip_instance_cache=True,
    )
    try:
        fs.mkdir("/one/two", create_parents=True)
        for directory in ("/", "/one", "/one/two"):
            fs.ls(directory)
        visited = []
        for directory, dirs, _ in fs.walk("/"):
            visited.append(directory)
            if directory == "/one":
                dirs.clear()
        assert visited == ["/", "/one"]
    finally:
        fs.close()


@pytest.mark.parametrize("operation", ["find", "du"])
def test_withdirs_reuses_the_root_metadata(dummy_fs, monkeypatch, operation):
    dummy_fs.mkdir("/root")
    dummy_fs.pipe_file("/root/entry", b"abc")
    expected = getattr(dummy_fs, operation)(
        "/root", withdirs=True, **({"total": False} if operation == "du" else {})
    )
    original = dummy_fs.info
    calls = []

    def record_info(path, **kwargs):
        calls.append(path)
        return original(path, **kwargs)

    monkeypatch.setattr(dummy_fs, "info", record_info)
    assert (
        getattr(dummy_fs, operation)(
            "/root", withdirs=True, **({"total": False} if operation == "du" else {})
        )
        == expected
    )
    assert calls.count("/root") == 1
