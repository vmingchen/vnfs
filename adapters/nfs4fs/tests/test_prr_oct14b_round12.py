"""Cache fills racing with a mutation cannot survive its completion."""

import errno

import fsspec
import pytest
from fsspec.core import OpenFile


def cache_for(fs, tmp_path):
    return fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )


def read_cached(cache, path="/a"):
    with cache.open(path, "rb") as file:
        return file.read()


def refill_before(monkeypatch, fs, method, refill, fail_after=None):
    original = getattr(fs._client, method)
    pending = True

    def call(*args, **kwargs):
        nonlocal pending
        if pending:
            pending = False
            refill()
        result = original(*args, **kwargs)
        if fail_after is not None:
            raise fail_after
        return result

    monkeypatch.setattr(fs._client, method, call)


@pytest.mark.parametrize(
    "operation", ["write", "append", "truncate", "group_write", "group_append"]
)
def test_descriptor_mutation_discards_inflight_persistent_refills(
    dummy_fs, tmp_path, monkeypatch, operation
):
    dummy_fs.pipe_file("/a", b"old!")
    grouped = operation.startswith("group_")
    mode = "ab" if "append" in operation else "wb" if grouped else "rb+"
    files = (
        dummy_fs.open_many([OpenFile(dummy_fs, "/a", mode)], write_buffering=True)
        if grouped
        else [dummy_fs.open("/a", mode, cache_type="none")]
    )
    if operation == "group_write":
        dummy_fs.pipe_file("/a", b"old!")
    cache = cache_for(dummy_fs, tmp_path)
    assert read_cached(cache) == b"old!"
    methods = {
        "write": "pwrite",
        "append": "write_positioned",
        "truncate": "ftruncate",
        "group_write": "pwrite_many",
        "group_append": "append_many",
    }
    refill_before(monkeypatch, dummy_fs, methods[operation], lambda: read_cached(cache))
    try:
        if operation == "truncate":
            files[0].truncate(2)
            expected = b"ol"
        else:
            files[0].write(b"new!")
            files[0].flush()
            expected = b"old!new!" if "append" in operation else b"new!"
        assert dummy_fs.cat_file("/a") == expected
        assert read_cached(cache) == expected
    finally:
        for file in files:
            file.close()


@pytest.mark.parametrize("operation", ["pipe", "open", "rename", "copy"])
def test_namespace_mutation_discards_inflight_refill(
    dummy_fs, tmp_path, monkeypatch, operation
):
    dummy_fs.pipe_file("/a", b"old!")
    dummy_fs.pipe_file("/source", b"new!")
    cache = cache_for(dummy_fs, tmp_path)
    assert read_cached(cache) == b"old!"
    method = {
        "pipe": "write_many",
        "open": "open",
        "rename": "rename_many",
        "copy": "copy_many",
    }[operation]
    refill_before(monkeypatch, dummy_fs, method, lambda: read_cached(cache))
    if operation == "pipe":
        dummy_fs.pipe_file("/a", b"new!")
    elif operation == "open":
        dummy_fs.open("/a", "wb").close()
    elif operation == "rename":
        dummy_fs.mv("/source", "/a")
    else:
        dummy_fs.cp_file("/source", "/a")
    expected = b"" if operation == "open" else b"new!"
    assert read_cached(cache) == dummy_fs.cat_file("/a") == expected


def test_listing_fill_during_mkdir_is_invalidated(dummy_fs, monkeypatch):
    dummy_fs.dircache.use_listings_cache = True
    dummy_fs.ls("/")
    refill_before(monkeypatch, dummy_fs, "mkdir_many", lambda: dummy_fs.ls("/"))
    dummy_fs.mkdir("/created")
    assert [info["name"] for info in dummy_fs.ls("/")] == ["nfs4:///created"]


def test_alias_of_renamed_descriptor_refill_is_invalidated(
    dummy_fs, tmp_path, monkeypatch
):
    dummy_fs.pipe_file("/a", b"old!")
    dummy_fs.hardlink("/a", "/alias")
    with dummy_fs.open("/a", "rb+", cache_type="none") as file:
        dummy_fs.mv("/a", "/renamed")
        cache = cache_for(dummy_fs, tmp_path)
        assert read_cached(cache, "/alias") == b"old!"
        refill_before(
            monkeypatch, dummy_fs, "pwrite", lambda: read_cached(cache, "/alias")
        )
        file.write(b"new!")
        assert read_cached(cache, "/alias") == b"new!"


def test_postmutation_cleanup_preserves_primary_error(dummy_fs, tmp_path, monkeypatch):
    dummy_fs.pipe_file("/a", b"old!")
    cache = cache_for(dummy_fs, tmp_path)
    primary = OSError(errno.EIO, "ambiguous mutation")
    with dummy_fs.open("/a", "rb+", cache_type="none") as file:
        refill_before(
            monkeypatch, dummy_fs, "pwrite", lambda: read_cached(cache), primary
        )
        with pytest.raises(OSError) as error:
            file.write(b"new!")
        assert error.value is primary
    assert read_cached(cache) == b"new!"


@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("close_fails", [False, True])
def test_post_open_invalidation_failure_releases_all_acquired_handles(
    dummy_fs, monkeypatch, mode, grouped, close_fails
):
    if mode != "xb":
        for path in ["/a", "/b"]:
            dummy_fs.pipe_file(path, b"old!")

    class Cache:
        pass

    cache = Cache()
    count = 0
    primary = OSError(errno.ENOSPC, "post-open metadata failed")
    cleanup = OSError(errno.EIO, "native CLOSE failed")

    def invalidate(*args, **kwargs):
        nonlocal count
        count += 1
        if count == 2:
            raise primary

    def fail_close(*args):
        raise cleanup

    dummy_fs._register_persistent_cache(cache, invalidate)
    with monkeypatch.context() as patch:
        if close_fails:
            patch.setattr(
                dummy_fs._client, "close_many" if grouped else "close", fail_close
            )
        with pytest.raises(OSError) as error:
            if grouped:
                dummy_fs.open_many(
                    [OpenFile(dummy_fs, path, mode) for path in ["/a", "/b"]]
                )
            else:
                dummy_fs.open("/a", mode)
        assert error.value is primary
        if close_fails:
            assert error.value.__cause__ is cleanup
            assert len(dummy_fs._client._fds) == (2 if grouped else 1)
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
    dummy_fs.info("/a")
    assert not dummy_fs._client._fds
