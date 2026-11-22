"""Complete persistent generations retain the native read allocation policy."""

import errno
import gzip
import io

import fsspec
import pytest
from fsspec.core import OpenFile
from vfsi_fsspec._fs import _OwnedStream


def complete_cache(fs, tmp_path, payload=b"abcdefghijk\n"):
    fs.pipe_file("/data", payload)
    cached = fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(tmp_path / "cache"),
        skip_instance_cache=True,
    )
    with cached.open("/data", "rb") as file:
        assert file.read() == payload
    assert cached._metadata.cached_files[-1]["/data"]["blocks"] is True
    return cached


def reader(cached, grouped):
    return (
        cached.open_many([OpenFile(cached, "/data", "rb")])[0]
        if grouped
        else cached.open("/data", "rb")
    )


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("method", ["read", "read1", "readline", "next", "readlines"])
def test_complete_uncompressed_cache_enforces_returned_byte_cap(
    dummy_fs, tmp_path, grouped, method
):
    cached = complete_cache(dummy_fs, tmp_path)
    dummy_fs.read_all_max_total_bytes = 4
    with reader(cached, grouped) as file:
        assert not dummy_fs._client._fds
        with pytest.raises(OSError) as error:
            if method == "next":
                next(file)
            else:
                getattr(file, method)()
        assert error.value.errno == errno.EFBIG
        assert error.value.filename == "/data"
        assert file.tell() == 0
        assert file.read(4) == b"abcd"
    assert file.closed


@pytest.mark.parametrize("grouped", [False, True])
def test_complete_cache_allows_chunking_and_active_limit_changes(
    dummy_fs, tmp_path, grouped
):
    cached = complete_cache(dummy_fs, tmp_path, b"abcdefghijkl")
    with reader(cached, grouped) as file:
        dummy_fs.read_all_max_total_bytes = 4
        assert file.read(4) == b"abcd"
        assert file.read(4) == b"efgh"
        dummy_fs.read_all_max_total_bytes = 2
        assert file.read(2) == b"ij"
        assert file.read(100) == b"kl"
        dummy_fs.read_all_max_total_bytes = 0
        assert file.read() == b""
        assert file.read(0) == b""
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("grouped", [False, True])
def test_complete_cache_zero_budget_rejects_nonempty_output(
    dummy_fs, tmp_path, grouped
):
    cached = complete_cache(dummy_fs, tmp_path)
    dummy_fs.read_all_max_total_bytes = 0
    with reader(cached, grouped) as file:
        assert file.read(0) == b""
        with pytest.raises(OSError) as error:
            file.read()
        assert error.value.errno == errno.EFBIG


def test_gzip_over_budget_readline_keeps_decoded_position(dummy_fs):
    limit = 1 << 18
    dummy_fs.pipe_file("/data.gz", gzip.compress(b"x" * (limit + 20) + b"\n"))
    dummy_fs.read_all_max_total_bytes = limit
    with dummy_fs.open("/data.gz", "rb", compression="gzip", cache_type="none") as file:
        assert file.read(10) == b"x" * 10
        with pytest.raises(OSError) as error:
            file.readline()
        assert error.value.errno == errno.EFBIG
        assert file.tell() == 10
        assert file.read(4) == b"xxxx"


def test_nonseekable_reader_reports_budget_error(dummy_fs):
    class NonSeekable(io.BytesIO):
        def seekable(self):
            return False

        def seek(self, *args):
            pytest.fail("seek called on a nonseekable stream")

    raw = NonSeekable(b"abcdef")
    dummy_fs.read_all_max_total_bytes = 4
    with _OwnedStream(raw, raw, fs=dummy_fs, path="/data") as file:
        with pytest.raises(OSError) as error:
            file.read()
        assert error.value.errno == errno.EFBIG
        assert error.value.filename == "/data"
