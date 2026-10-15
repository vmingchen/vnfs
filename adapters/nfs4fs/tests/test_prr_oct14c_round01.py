"""Single-file downloads preserve every byte and bound remote read buffers."""

import errno
import io

import pytest
from fsspec.callbacks import Callback


class ShortOutput(io.BytesIO):
    def __init__(self, accepted):
        super().__init__()
        self.accepted = accepted

    def write(self, data):
        return super().write(data[: self.accepted])


@pytest.mark.parametrize("accepted", [1, 2, 3])
@pytest.mark.parametrize("explicit", [False, True])
def test_get_file_completes_short_output_writes(dummy_fs, accepted, explicit):
    dummy_fs.pipe_file("/source", b"abcdefgh")
    output = ShortOutput(accepted)
    callback = Callback()
    if explicit:
        dummy_fs.get_file("/source", outfile=output, callback=callback)
    else:
        dummy_fs.get_file("/source", output, callback=callback)
    assert output.getvalue() == b"abcdefgh"
    assert callback.size == callback.value == 8
    assert not output.closed
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("progress", [0, -1, 100])
def test_get_file_rejects_invalid_write_progress(dummy_fs, progress):
    dummy_fs.pipe_file("/source", b"abcdefgh")

    class InvalidOutput(io.BytesIO):
        def write(self, data):
            return progress

    output = InvalidOutput()
    with pytest.raises(OSError) as error:
        dummy_fs.get_file("/source", output)
    assert error.value.errno == errno.EIO
    assert not output.closed
    assert not dummy_fs._client._fds


def test_get_file_preserves_none_write_result_compatibility(dummy_fs):
    dummy_fs.pipe_file("/source", b"abcdefgh")

    class NoneOutput(io.BytesIO):
        def write(self, data):
            super().write(data)
            return None

    output = NoneOutput()
    callback = Callback()
    dummy_fs.get_file("/source", output, callback=callback)
    assert output.getvalue() == b"abcdefgh"
    assert callback.size == callback.value == 8
    assert not output.closed


@pytest.mark.parametrize("filelike", [False, True])
def test_get_file_streams_larger_than_allocation_limit(dummy_fs, tmp_path, filelike):
    data = b"abcdefghijk"
    dummy_fs.pipe_file("/source", data)
    dummy_fs.read_all_max_total_bytes = 4
    output = io.BytesIO() if filelike else tmp_path / "parent" / "target"
    callback = Callback()
    dummy_fs.get_file("/source", output, callback=callback)
    assert (output.getvalue() if filelike else output.read_bytes()) == data
    assert callback.size == callback.value == len(data)
    assert not dummy_fs._client._fds


def test_get_file_callback_setup_failure_preserves_local_file(dummy_fs, tmp_path):
    dummy_fs.pipe_file("/source", b"new")
    output = tmp_path / "target"
    output.write_bytes(b"original")

    class BrokenCallback(Callback):
        def set_size(self, size):
            raise RuntimeError("callback setup failed")

    with pytest.raises(RuntimeError, match="callback setup failed"):
        dummy_fs.get_file("/source", output, callback=BrokenCallback())
    assert output.read_bytes() == b"original"
    assert not dummy_fs._client._fds


def test_get_file_missing_remote_preserves_local_file(dummy_fs, tmp_path):
    output = tmp_path / "target"
    output.write_bytes(b"original")
    with pytest.raises(FileNotFoundError):
        dummy_fs.get_file("/missing", output)
    assert output.read_bytes() == b"original"
    assert not dummy_fs._client._fds


def test_get_file_output_failure_remains_primary_on_close_failure(
    dummy_fs, monkeypatch
):
    dummy_fs.pipe_file("/source", b"abcdefgh")

    class BrokenOutput(io.BytesIO):
        def write(self, data):
            raise ValueError("output failed")

    def broken_close(fd):
        raise OSError(errno.EIO, "close failed")

    output = BrokenOutput()
    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close", broken_close)
        with pytest.raises(ValueError, match="output failed"):
            dummy_fs.get_file("/source", output)
    assert not output.closed
    dummy_fs._client.shutdown()
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("cap", [0, 1, 4])
def test_get_file_empty_source_controls(dummy_fs, tmp_path, cap):
    dummy_fs.pipe_file("/empty", b"")
    dummy_fs.read_all_max_total_bytes = cap
    output = tmp_path / "target"
    callback = Callback()
    dummy_fs.get_file("/empty", output, callback=callback)
    assert output.read_bytes() == b""
    assert callback.value == callback.size == 0
    assert not dummy_fs._client._fds


def test_get_file_directory_control(dummy_fs, tmp_path):
    dummy_fs.mkdir("/directory")
    output = tmp_path / "parent" / "directory"
    dummy_fs.get_file("/directory", output)
    assert output.is_dir()


@pytest.mark.parametrize("compression", ["gzip", "bz2"])
def test_get_file_preserves_compression_options(dummy_fs, compression):
    with dummy_fs.open("/encoded", "wb", compression=compression) as stream:
        stream.write(b"abcdefgh")
    output = ShortOutput(2)
    callback = Callback()
    dummy_fs.get_file("/encoded", output, compression=compression, callback=callback)
    assert output.getvalue() == b"abcdefgh"
    assert callback.value == 8
    assert not output.closed
    assert not dummy_fs._client._fds


def test_get_file_nonempty_zero_cap_fails_without_leaking(dummy_fs):
    dummy_fs.pipe_file("/source", b"data")
    dummy_fs.read_all_max_total_bytes = 0
    output = io.BytesIO()
    with pytest.raises(OSError) as error:
        dummy_fs.get_file("/source", output)
    assert error.value.errno == errno.EFBIG
    assert output.getvalue() == b""
    assert not output.closed
    assert not dummy_fs._client._fds
