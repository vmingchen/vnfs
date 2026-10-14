"""Constructor failures retain neither native handles nor half-built writers."""

import errno

import pytest
from vfsi_fsspec import _fs


@pytest.mark.parametrize("mode", ["wb", "ab", "xb", "rb", "wb+", "ab+", "xb+", "rb+"])
def test_invalid_cache_options_fail_before_native_mutation(dummy_fs, mode):
    if not mode.startswith("x"):
        dummy_fs.pipe_file("/target", b"original")
    with pytest.raises(TypeError) as error:
        dummy_fs.open("/target", mode, cache_options=1)
    assert error.value is not None
    assert not dummy_fs._client._fds
    if mode.startswith("x"):
        assert not dummy_fs.exists("/target")
    else:
        assert dummy_fs.cat_file("/target") == b"original"


@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
@pytest.mark.parametrize("close_fails", [False, True])
def test_buffer_allocation_failure_cleans_up_raw_owner(
    dummy_fs, monkeypatch, mode, close_fails
):
    if mode != "xb":
        dummy_fs.pipe_file("/target", b"original")
    primary = MemoryError("buffer allocation failed")
    cleanup = OSError(errno.EIO, "CLOSE failed")
    facades = []
    original_init = _fs.AbstractBufferedFile.__init__

    def fail_buffer(file, *args, **kwargs):
        facades.append(file)
        original_init(file, *args, **kwargs)
        raise primary

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(_fs.AbstractBufferedFile, "__init__", fail_buffer)
        if close_fails:
            patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises(MemoryError) as error:
            dummy_fs.open("/target", mode, write_buffering=True)
        assert error.value is primary
        assert facades[0].closed
        if close_fails:
            assert error.value.__cause__ is cleanup
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
            assert facades[0]._raw.closed
        facades[0].close()  # No flush or second finalization of partial setup.
    dummy_fs.info("/target")
    assert not dummy_fs._client._fds


def test_actual_bytes_buffer_allocation_failure_closes_handle(dummy_fs, monkeypatch):
    def fail(*args, **kwargs):
        raise MemoryError("BytesIO allocation failed")

    with monkeypatch.context() as patch:
        patch.setattr(_fs.io, "BytesIO", fail)
        with pytest.raises(MemoryError) as error:
            dummy_fs.open("/target", "wb", write_buffering=True)
        assert error.value is not None
        assert not dummy_fs._client._fds


@pytest.mark.parametrize("buffered", [False, True])
def test_successful_constructor_retains_owner_until_close(dummy_fs, buffered):
    file = dummy_fs.open("/target", "wb", write_buffering=buffered)
    assert not file.closed
    assert len(dummy_fs._client._fds) == 1
    file.write(b"valid")
    file.close()
    assert file.closed and not dummy_fs._client._fds
    assert dummy_fs.cat_file("/target") == b"valid"


@pytest.mark.parametrize("mode", ["rb", "wb", "ab"])
@pytest.mark.parametrize("failure_index", [0, 1])
def test_group_constructor_failure_closes_every_remaining_token(
    dummy_fs, monkeypatch, mode, failure_index
):
    from fsspec.core import OpenFile

    for path in ["/a", "/b", "/c"]:
        dummy_fs.pipe_file(path, b"original")
    original_init = _fs.AbstractBufferedFile.__init__
    facades = []
    primary = MemoryError("group buffer allocation failed")

    def fail_selected(file, *args, **kwargs):
        facades.append(file)
        original_init(file, *args, **kwargs)
        if len(facades) - 1 == failure_index:
            raise primary

    monkeypatch.setattr(_fs.AbstractBufferedFile, "__init__", fail_selected)
    with pytest.raises(MemoryError) as error:
        dummy_fs.open_many(
            [OpenFile(dummy_fs, path, mode) for path in ["/a", "/b", "/c"]],
            write_buffering=True,
        )
    assert error.value is primary
    assert error.value.__cause__ is None
    assert not dummy_fs._client._fds
    assert not dummy_fs._client._deferred_close
    assert facades[-1].closed
