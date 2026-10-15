"""Direct mmap caching must not publish unfilled blocks or retain mappings."""

import errno

import pytest
from fsspec.core import OpenFile


def open_readers(fs, grouped):
    if grouped:
        return fs.open_many(
            [OpenFile(fs, "/a", "rb"), OpenFile(fs, "/b", "rb")],
            cache_type="mmap",
            block_size=2,
        )
    return [fs.open("/a", "rb", cache_type="mmap", block_size=2)]


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("fault", ["error", "empty", "oversized"])
def test_failed_mmap_fetch_is_not_published_and_retry_reads_real_data(
    dummy_fs, monkeypatch, grouped, fault
):
    dummy_fs.vectorized_buffering = False
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, b"abcdef")
    files = open_readers(dummy_fs, grouped)
    file = files[0]

    def fail(*args, **kwargs):
        if fault == "error":
            raise OSError(errno.EIO, "fetch failed")
        return b"" if fault == "empty" else b"x" * 20

    try:
        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, "pread", fail)
            with pytest.raises((OSError, ValueError)) as error:
                file.read(1)
            assert error.value is not None
            assert not file.cache.blocks
            assert file.tell() == 0
        assert file.read(1) == b"a"
        assert file.read() == b"bcdef"
        if grouped:
            assert files[1].read() == b"abcdef"
    finally:
        for file in files:
            file.close()
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("failing_read", [False, True])
def test_direct_mmap_is_closed_while_cache_and_error_are_retained(
    dummy_fs, monkeypatch, grouped, failing_read
):
    dummy_fs.vectorized_buffering = False
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, b"abcdef")
    files = open_readers(dummy_fs, grouped)
    caches = [file.cache for file in files]
    if failing_read:

        def fail(*args):
            raise OSError(errno.EIO, "read failed")

        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, "pread", fail)
            with pytest.raises(OSError) as error:
                files[0].read(1)
            assert error.value is not None
    else:
        assert files[0].read(1) == b"a"
    for file in files:
        file.close()
    assert all(cache.cache.closed for cache in caches)
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("grouped", [False, True])
def test_empty_direct_mmap_remains_readable_and_closeable(dummy_fs, grouped):
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, b"")
    files = open_readers(dummy_fs, grouped)
    for file in files:
        assert file.read() == b""
        file.close()
        file.close()
    assert not dummy_fs._client._fds


def test_vectorized_group_mmap_failure_is_retryable(dummy_fs, monkeypatch):
    for path in ["/a", "/b"]:
        dummy_fs.pipe_file(path, b"abcdef")
    files = open_readers(dummy_fs, True)
    assert dummy_fs.vectorized_buffering

    def fail(*args):
        raise OSError(errno.EIO, "vector fetch failed")

    try:
        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, "pread_many", fail)
            with pytest.raises(OSError):
                files[0].read(1)
            assert all(not file.cache.blocks for file in files)
        assert [file.read() for file in files] == [b"abcdef", b"abcdef"]
    finally:
        for file in files:
            file.close()
    assert not dummy_fs._client._fds


def test_mmap_exclusive_end_and_global_registry_are_preserved(dummy_fs):
    from fsspec.caching import MMapCache, caches
    from vfsi_fsspec._caching import _Nfs4MMapCache

    assert caches["mmap"] is MMapCache
    dummy_fs.pipe_file("/a", b"abcdef")
    with dummy_fs.open("/a", "rb", cache_type="mmap", block_size=2) as file:
        assert isinstance(file.cache, _Nfs4MMapCache)
        assert file.read(2) == b"ab"
        assert file.cache.blocks == {0}
    with dummy_fs.open("/a", "rb", cache_type="readahead", block_size=2) as file:
        assert not isinstance(file.cache, _Nfs4MMapCache)
        assert file.read() == b"abcdef"
    assert not dummy_fs._client._fds
