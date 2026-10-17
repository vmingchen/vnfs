"""Codec lookahead cannot return more bytes than the read budget."""

import bz2
import errno
import gzip
import lzma

import fsspec
import pytest
from fsspec.core import OpenFile

# Keep the cap below codec peek buffers on all supported Python versions.
# peek's hint is not a minimum; older codecs legitimately return only 8 KiB.
CAP = 1 << 10
CODECS = {"gzip": gzip.compress, "bz2": bz2.compress, "xz": lzma.compress}


def open_codec(fs, tmp_path, codec, route, payload):
    encoded = CODECS[codec](payload)
    fs.pipe_file("/data", encoded)
    if route == "grouped":
        return fs.open_many([OpenFile(fs, "/data", "rb", compression=codec)])[0]
    if route in ("cached", "complete"):
        cached = fsspec.filesystem(
            "blockcache",
            fs=fs,
            cache_storage=str(tmp_path / "cache"),
            skip_instance_cache=True,
        )
        if route == "complete":
            with cached.open("/data", "rb") as file:
                assert file.read() == encoded
        return cached.open("/data", "rb", compression=codec)
    return fs.open("/data", "rb", compression=codec)


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("route", ["scalar", "grouped", "cached", "complete"])
@pytest.mark.parametrize("hint", [0, 1, CAP * 2])
def test_codec_peek_cannot_exceed_returned_byte_limit(
    dummy_fs, tmp_path, codec, route, hint
):
    dummy_fs.read_all_max_total_bytes = CAP
    with open_codec(dummy_fs, tmp_path, codec, route, b"x" * (CAP * 4)) as file:
        with pytest.raises(OSError) as error:
            file.peek(hint)
        assert error.value.errno == errno.EFBIG
        assert error.value.filename == "/data"
        assert file.tell() == 0
        assert file.read(1) == b"x"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("route", ["scalar", "grouped", "cached", "complete"])
def test_peek_keeps_logical_position_and_normal_read_behavior(
    dummy_fs, tmp_path, codec, route
):
    dummy_fs.read_all_max_total_bytes = CAP
    payload = b"prefix\nsuffix"
    with open_codec(dummy_fs, tmp_path, codec, route, payload) as file:
        assert payload.startswith(file.peek(1))
        assert file.tell() == 0
        assert file.read(3) == b"pre"
        assert payload[3:].startswith(file.peek(0))
        assert file.tell() == 3
        assert file.readline() == b"fix\n"
        assert file.read() == b"suffix"
    with pytest.raises(ValueError):
        file.peek(1)
    assert not dummy_fs._client._fds


def test_plain_complete_cache_peek_uses_the_same_guard(dummy_fs, tmp_path):
    dummy_fs.pipe_file("/data", b"abcdefgh")
    cached = fsspec.filesystem(
        "blockcache",
        fs=dummy_fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )
    with cached.open("/data", "rb") as file:
        assert file.read() == b"abcdefgh"
    dummy_fs.read_all_max_total_bytes = 4
    with cached.open("/data", "rb") as file:
        with pytest.raises(OSError) as error:
            file.peek(1)
        assert error.value.errno == errno.EFBIG
        assert file.tell() == 0
        assert file.read(1) == b"a"
