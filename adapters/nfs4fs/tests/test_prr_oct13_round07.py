"""PRR round 7: failed cache setup releases every acquired resource."""

import builtins
import errno
import os
import weakref

import fsspec
import pytest
from fsspec.core import OpenFile
from vfsi_fsspec import _blockcache


def cached_fs(fs, tmp_path):
    return fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )


def test_later_complete_cache_open_failure_closes_earlier_file(
    dummy_fs, tmp_path, monkeypatch
):
    cached = cached_fs(dummy_fs, tmp_path)
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, b"content")
        with cached.open(path, "rb") as file:
            assert file.read() == b"content"
    records = cached._metadata.cached_files[-1]
    first = _blockcache._backing_path(cached, records["/a"])
    second = _blockcache._backing_path(cached, records["/b"])
    references, descriptors = [], []
    primary = PermissionError(errno.EACCES, "cache OPEN failed", second)
    original = builtins.open

    def open_cache(path, *args, **kwargs):
        if str(path) == second and args == ("rb",):
            raise primary
        file = original(path, *args, **kwargs)
        if str(path) == first and args == ("rb",):
            references.append(weakref.ref(file))
            descriptors.append(file.fileno())
        return file

    monkeypatch.setattr(builtins, "open", open_cache)
    with pytest.raises(PermissionError) as error:
        cached.open_many([OpenFile(cached, "/a"), OpenFile(cached, "/b")])
    assert error.value is primary
    assert references[0]() is None or references[0]().closed
    with pytest.raises(OSError) as closed:
        os.fstat(descriptors[0])
    assert closed.value.errno == errno.EBADF
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("close_fails", [False, True])
def test_scalar_metadata_failure_releases_native_and_mapping(
    dummy_fs, tmp_path, monkeypatch, close_fails
):
    dummy_fs.pipe_file("/a", b"content")
    cached = cached_fs(dummy_fs, tmp_path)
    primary = OSError(errno.ENOSPC, "cache metadata full")
    cleanup = OSError(errno.EIO, "native CLOSE failed")
    references, mappings = [], []
    mark = _blockcache._mark_compatible_cache

    def capture_mark(cache_fs, path, file):
        references.append(weakref.ref(file))
        mappings.append(file.cache.cache)
        return mark(cache_fs, path, file)

    def fail_save(*args):
        raise primary

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(_blockcache, "_mark_compatible_cache", capture_mark)
        patch.setattr(_blockcache, "_save_writable_metadata", fail_save)
        if close_fails:
            patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises(OSError) as error:
            cached.open("/a", "rb")
        assert error.value is primary
        assert mappings[0].closed
        if close_fails:
            assert error.value.__cause__ is cleanup
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
            assert references[0]().closed
    dummy_fs.info("/a")
    assert not dummy_fs._client._fds
    with cached.open("/a", "rb") as file:
        assert file.read() == b"content"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("close_fails", [False, True])
def test_group_metadata_failure_releases_all_tokens(
    dummy_fs, tmp_path, monkeypatch, close_fails
):
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, b"content")
    cached = cached_fs(dummy_fs, tmp_path)
    primary = OSError(errno.ENOSPC, "cache metadata full")
    cleanup = OSError(errno.EIO, "group CLOSE failed")
    save = _blockcache._save_writable_metadata
    calls = []
    mappings = []
    constructor = _blockcache._Nfs4MMapCache

    def make_mapping(*args, **kwargs):
        cache = constructor(*args, **kwargs)
        mappings.append(cache.cache)
        return cache

    def fail_final_save(*args):
        calls.append(True)
        if len(calls) == 2:
            raise primary
        return save(*args)

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(_blockcache, "_save_writable_metadata", fail_final_save)
        patch.setattr(_blockcache, "_Nfs4MMapCache", make_mapping)
        if close_fails:
            patch.setattr(dummy_fs._client, "close_many", fail_close)
        with pytest.raises(OSError) as error:
            cached.open_many([OpenFile(cached, "/a"), OpenFile(cached, "/b")])
        assert error.value is primary
        assert len(mappings) == 2 and all(mapping.closed for mapping in mappings)
        if close_fails:
            assert error.value.__cause__ is cleanup
            assert len(dummy_fs._client._fds) == 2
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
    dummy_fs.info("/a")
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("complete", [0, 1, 2])
def test_successful_cache_group_retains_resources_until_close(
    dummy_fs, tmp_path, complete
):
    cached = cached_fs(dummy_fs, tmp_path)
    for index, path in enumerate(["/a", "/b"]):
        dummy_fs.pipe_file(path, b"content")
        if index < complete:
            with cached.open(path, "rb") as file:
                assert file.read() == b"content"
    files = cached.open_many([OpenFile(cached, "/a"), OpenFile(cached, "/b")])
    assert all(not file.closed for file in files)
    assert [file.read() for file in files] == [b"content", b"content"]
    for file in files:
        file.close()
    assert not dummy_fs._client._fds
