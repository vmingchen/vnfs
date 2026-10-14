"""PRR round 9: scalar compression owns its underlying file until close."""

import errno
import io

import pytest
from vfsi_fsspec import _fs

CODECS = [
    pytest.param(
        codec,
        marks=pytest.mark.skipif(codec not in _fs.compr, reason=f"{codec} unavailable"),
    )
    for codec in ["gzip", "bz2", "lzma", "xz", "zstd", "zip"]
]
PAYLOAD = b"first line\nsecond line\n"


def archive(codec):
    output = io.BytesIO()
    with _fs.compr[codec](output, mode="w") as writer:
        writer.write(PAYLOAD)
    return output.getvalue()


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("mode", ["rb", "rt"])
@pytest.mark.parametrize("context", [False, True])
def test_closed_compressed_reader_releases_raw(dummy_fs, codec, mode, context):
    dummy_fs.pipe_file("/archive", archive(codec))
    reader = dummy_fs.open("/archive", mode, compression=codec)
    if context:
        with reader:
            assert reader.read() == (PAYLOAD if mode == "rb" else PAYLOAD.decode())
    else:
        assert reader.read() == (PAYLOAD if mode == "rb" else PAYLOAD.decode())
        reader.close()
    assert reader.closed
    assert not dummy_fs._client._fds
    reader.close()
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("transactional", [False, True])
def test_compressed_writer_finalizes_before_raw_close(dummy_fs, codec, transactional):
    def write():
        with dummy_fs.open("/archive", "wt", compression=codec) as writer:
            writer.write(PAYLOAD.decode())
        assert writer.closed

    if transactional:
        with dummy_fs.transaction:
            write()
            assert not dummy_fs.exists("/archive")
    else:
        write()
    with dummy_fs.open("/archive", "rt", compression=codec) as reader:
        assert reader.read() == PAYLOAD.decode()
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("body_fails", [False, True])
@pytest.mark.parametrize("mode", ["rb", "rt"])
def test_compressed_close_failure_is_deferred_and_body_error_preserved(
    dummy_fs, monkeypatch, body_fails, mode
):
    dummy_fs.pipe_file("/archive", archive("gzip"))
    cleanup = OSError(errno.EIO, "native CLOSE failed")
    primary = RuntimeError("application failed")
    reader = dummy_fs.open("/archive", mode, compression="gzip")

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises(RuntimeError if body_fails else OSError) as error:
            with reader:
                assert reader.read() == (PAYLOAD if mode == "rb" else PAYLOAD.decode())
                if body_fails:
                    raise primary
        assert error.value is (primary if body_fails else cleanup)
        if body_fails:
            assert error.value.__cause__ is cleanup
        assert reader.closed
        assert len(dummy_fs._client._fds) == 1
        assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
    dummy_fs.info("/archive")
    assert not dummy_fs._client._fds
    reader.close()


@pytest.mark.parametrize("raw_close_fails", [False, True])
def test_codec_finalization_error_still_closes_raw(
    dummy_fs, monkeypatch, raw_close_fails
):
    dummy_fs.pipe_file("/source", b"content")
    primary = RuntimeError("codec finalization failed")
    cleanup = OSError(errno.EIO, "native CLOSE failed")

    class Codec:
        def __init__(self, file, **kwargs):
            self.file = file

        def close(self):
            raise primary

        def __getattr__(self, name):
            return getattr(self.file, name)

    def fail_close(*args):
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setitem(_fs.compr, "gzip", Codec)
        reader = dummy_fs.open("/source", "rb", compression="gzip")
        if raw_close_fails:
            patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises(RuntimeError) as error:
            reader.close()
        assert error.value is primary
        assert reader.closed
        if raw_close_fails:
            assert error.value.__cause__ is cleanup
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
        else:
            assert not dummy_fs._client._fds
        reader.close()  # Do not repeat a failed codec finalization.
    dummy_fs.info("/source")
    assert not dummy_fs._client._fds


def test_compression_proxy_preserves_binary_file_interface(dummy_fs):
    dummy_fs.pipe_file("/archive", archive("gzip"))
    with dummy_fs.open("/archive", "rb", compression="gzip") as reader:
        assert isinstance(reader, io.IOBase)
        assert reader.readable() and not reader.writable() and reader.seekable()
        buffer = bytearray(5)
        assert reader.readinto(buffer) == 5
        assert buffer == b"first"
        assert reader.tell() == 5
        assert reader.seek(0) == 0
        assert next(reader) == b"first line\n"
        assert reader.readlines() == [b"second line\n"]
    with pytest.raises(ValueError):
        reader.read(1)
    assert not dummy_fs._client._fds
