"""Grouped mmap ownership covers commit and every failed attach member."""

import errno

import pytest
from fsspec.core import OpenFile
from vfsi_fsspec import _fs


def open_group(fs):
    return fs.open_many(
        [OpenFile(fs, path, "rb") for path in ["/a", "/b", "/c"]],
        cache_type="mmap",
        block_size=2,
    )


@pytest.mark.parametrize("empty", [False, True])
def test_group_commit_releases_all_retained_mmaps(dummy_fs, empty):
    payload = b"" if empty else b"abcdef"
    for path in ["/a", "/b", "/c"]:
        dummy_fs.pipe_file(path, payload)
    files = open_group(dummy_fs)
    caches = [file.cache for file in files]
    assert [file.read() for file in files] == [payload] * 3
    dummy_fs.commit_many(files)
    assert all(file.closed for file in files)
    if not empty:
        assert all(cache.cache.closed for cache in caches)
    assert not dummy_fs._client._fds
    dummy_fs.commit_many(files)


@pytest.mark.parametrize("failure_index", [0, 1, 2])
@pytest.mark.parametrize("close_fails", [False, True])
def test_later_mmap_attach_failure_releases_previous_mappings(
    dummy_fs, monkeypatch, failure_index, close_fails
):
    for path in ["/a", "/b", "/c"]:
        dummy_fs.pipe_file(path, b"abcdef")
    factory = _fs._Nfs4MMapCache
    mappings = []
    calls = 0
    primary = MemoryError("mmap construction failed")
    cleanup = OSError(errno.EIO, "native CLOSE failed")

    def fail_selected(*args, **kwargs):
        nonlocal calls
        index = calls
        calls += 1
        if index == failure_index:
            raise primary
        cache = factory(*args, **kwargs)
        mappings.append(cache.cache)
        return cache

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(_fs, "_Nfs4MMapCache", fail_selected)
        if close_fails:
            patch.setattr(dummy_fs._client, "close_many", fail_close)
        with pytest.raises(MemoryError) as error:
            open_group(dummy_fs)
        assert error.value is primary
        assert len(mappings) == failure_index
        assert all(mapping.closed for mapping in mappings)
        if close_fails:
            assert error.value.__cause__ is cleanup
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
    dummy_fs.info("/a")
    assert not dummy_fs._client._fds


def test_mapping_close_failure_does_not_skip_sibling_cleanup_and_can_retry(
    dummy_fs, monkeypatch
):
    for path in ["/a", "/b", "/c"]:
        dummy_fs.pipe_file(path, b"abcdef")
    files = open_group(dummy_fs)
    caches = [file.cache for file in files]
    close = caches[0].close
    primary = OSError(errno.EIO, "mapping close failed")
    attempts = 0
    native_calls = []
    close_many = dummy_fs._client.close_many

    def fail_once():
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise primary
        close()

    def record(tokens):
        native_calls.append(list(tokens))
        return close_many(tokens)

    monkeypatch.setattr(caches[0], "close", fail_once)
    monkeypatch.setattr(dummy_fs._client, "close_many", record)
    with pytest.raises(OSError) as error:
        dummy_fs.commit_many(files)
    assert error.value is primary
    assert all(cache.cache.closed for cache in caches[1:])
    assert not dummy_fs._client._fds
    dummy_fs.commit_many(files)
    assert all(cache.cache.closed for cache in caches)
    assert len(native_calls) == 1


def test_attach_primary_survives_local_mapping_cleanup_error(dummy_fs, monkeypatch):
    for path in ["/a", "/b", "/c"]:
        dummy_fs.pipe_file(path, b"abcdef")
    factory = _fs._Nfs4MMapCache
    caches = []
    primary = MemoryError("third mmap failed")
    cleanup = OSError(errno.EIO, "first mapping close failed")

    def construct(*args, **kwargs):
        if len(caches) == 2:
            raise primary
        cache = factory(*args, **kwargs)
        if not caches:
            close = cache.close

            def fail_after_close():
                close()
                raise cleanup

            cache.close = fail_after_close
        caches.append(cache)
        return cache

    monkeypatch.setattr(_fs, "_Nfs4MMapCache", construct)
    with pytest.raises(MemoryError) as error:
        open_group(dummy_fs)
    assert error.value is primary
    assert error.value.__cause__ is cleanup
    assert all(cache.cache.closed for cache in caches)
    assert not dummy_fs._client._fds
