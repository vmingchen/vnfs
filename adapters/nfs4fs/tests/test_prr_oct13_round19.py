"""Compressed output obeys the same returned-bytes budget as native reads."""

import bz2
import errno
import gzip
import lzma

import pytest
from fsspec.core import OpenFile

CAP = 1 << 20
CODECS = {"gzip": gzip.compress, "bz2": bz2.compress, "xz": lzma.compress}


def open_compressed(fs, codec, grouped):
    if grouped:
        return fs.open_many([OpenFile(fs, "/data", "rb", compression=codec)])[0]
    return fs.open("/data", "rb", compression=codec)


def invoke(file, method, size):
    if method == "next":
        return next(file)
    if method == "readlines":
        return file.readlines()[0]
    return getattr(file, method)() if size == "default" else getattr(file, method)(size)


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize(
    "method,size",
    [
        ("read", "default"),
        ("read", None),
        ("read", CAP + 257),
        ("read1", CAP + 257),
        ("readline", "default"),
        ("next", "default"),
        ("readlines", "default"),
    ],
)
def test_decoded_output_cannot_bypass_read_budget(
    dummy_fs, codec, grouped, method, size
):
    dummy_fs.read_all_max_total_bytes = CAP
    dummy_fs.pipe_file("/data", CODECS[codec](b"x" * (CAP + 257)))
    with open_compressed(dummy_fs, codec, grouped) as file:
        with pytest.raises(OSError) as error:
            invoke(file, method, size)
        assert error.value.errno == errno.EFBIG
        assert error.value.filename == "/data"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("payload", [b"", b"small\n", b"x" * CAP])
def test_at_or_below_cap_returns_short_eof_successfully(
    dummy_fs, codec, grouped, payload
):
    dummy_fs.read_all_max_total_bytes = CAP
    dummy_fs.pipe_file("/data", CODECS[codec](payload))
    with open_compressed(dummy_fs, codec, grouped) as file:
        assert file.read(CAP + 10) == payload
        assert file.read() == b""
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("codec", CODECS)
@pytest.mark.parametrize("grouped", [False, True])
def test_chunked_decoding_is_allowed_across_many_budgets(dummy_fs, codec, grouped):
    dummy_fs.read_all_max_total_bytes = CAP
    payload = b"x" * (CAP * 2)
    dummy_fs.pipe_file("/data", CODECS[codec](payload))
    with open_compressed(dummy_fs, codec, grouped) as file:
        assert file.read(0) == b""
        assert file.read(CAP) == payload[:CAP]
        assert file.read(CAP) == payload[CAP:]
        assert file.read() == b""
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("codec", CODECS)
def test_decoding_observes_cap_changed_after_open(dummy_fs, codec):
    dummy_fs.pipe_file("/data", CODECS[codec](b"x" * (CAP + 10)))
    with open_compressed(dummy_fs, codec, False) as file:
        dummy_fs.read_all_max_total_bytes = CAP
        with pytest.raises(OSError) as error:
            file.read()
        assert error.value.errno == errno.EFBIG


def test_codec_receives_bounded_requests_before_output_allocation(dummy_fs):
    import io

    from vfsi_fsspec._fs import _OwnedStream

    dummy_fs.read_all_max_total_bytes = 16
    calls = []

    class Codec(io.BytesIO):
        def read(self, size=-1):
            calls.append(size)
            assert 0 <= size <= 17
            return super().read(size)

    owner = dummy_fs.open("/data", "wb")
    with _OwnedStream(Codec(b"x" * 100), owner) as file:
        with pytest.raises(OSError) as error:
            file.read()
        assert error.value.errno == errno.EFBIG
    assert calls == [17]
    assert not dummy_fs._client._fds
