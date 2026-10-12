"""Whole-walk limits and one-fetch-per-directory regressions."""

from collections import Counter
import errno

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
