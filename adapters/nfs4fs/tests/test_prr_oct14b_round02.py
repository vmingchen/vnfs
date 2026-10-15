"""Bounded binary readline follows Python's standard file-object contract."""

import errno
import io

import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("cache", ["none", "readahead", "mmap"])
@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("size", [0, 1, 3, 7, 100, None, -2])
def test_readline_size_matches_python_file(dummy_fs, cache, grouped, size):
    payload = b"abcdef\nnext\n"
    dummy_fs.pipe_file("/data", payload)
    files = (
        dummy_fs.open_many(
            [OpenFile(dummy_fs, "/data", "rb")], cache_type=cache, block_size=2
        )
        if grouped
        else [dummy_fs.open("/data", "rb", cache_type=cache, block_size=2)]
    )
    oracle = io.BytesIO(payload)
    try:
        for _ in range(3):
            assert files[0].readline(size) == oracle.readline(size)
            assert files[0].tell() == oracle.tell()
        assert files[0].read() == oracle.read()
    finally:
        for file in files:
            file.close()
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("cache", ["none", "readahead", "mmap"])
def test_bounded_readline_can_stream_a_line_larger_than_budget(dummy_fs, cache):
    dummy_fs.pipe_file("/data", b"abcdefgh\n")
    dummy_fs.read_all_max_total_bytes = 4
    with dummy_fs.open("/data", "rb", cache_type=cache, block_size=2) as file:
        assert file.readline(3) == b"abc"
        assert file.readline(3) == b"def"
        assert file.readline(3) == b"gh\n"
        assert file.readline(3) == b""


@pytest.mark.parametrize("size", [None, -1, 20])
def test_readline_size_does_not_disable_allocation_limit(dummy_fs, size):
    dummy_fs.pipe_file("/data", b"abcdefgh\n")
    dummy_fs.read_all_max_total_bytes = 4
    with dummy_fs.open("/data", "rb", cache_type="none") as file:
        with pytest.raises(OSError) as error:
            file.readline(size)
        assert error.value.errno == errno.EFBIG


@pytest.mark.parametrize("size", ["3", 1.5])
def test_invalid_readline_size_does_not_consume_data(dummy_fs, size):
    dummy_fs.pipe_file("/data", b"abc\n")
    with dummy_fs.open("/data", "rb") as file:
        with pytest.raises(TypeError):
            file.readline(size)
        assert file.tell() == 0
        assert file.readline() == b"abc\n"
