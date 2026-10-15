"""Buffered append positions follow acknowledged append ends, not predictions."""

import pytest
from fsspec.core import OpenFile


def writer(fs, grouped, block_size):
    if grouped:
        return fs.open_many(
            [OpenFile(fs, "/data", "ab")], write_buffering=True, block_size=block_size
        )[0]
    return fs.open("/data", "ab", write_buffering=True, block_size=block_size)


@pytest.mark.parametrize("grouped", [False, True])
@pytest.mark.parametrize("block_size", [2, 100])
@pytest.mark.parametrize("external", [b"", b"external"])
def test_append_flush_and_default_truncate_preserve_acknowledged_data(
    dummy_fs, grouped, block_size, external
):
    dummy_fs.pipe_file("/data", b"old")
    file = writer(dummy_fs, grouped, block_size)
    try:
        with dummy_fs.open("/data", "ab") as other:
            other.write(external)
        file.write(b"ours!")
        file.flush()
        expected = b"old" + external + b"ours!"
        assert dummy_fs.cat_file("/data") == expected
        assert file.tell() == len(expected)
        assert file.truncate() == len(expected)
        assert dummy_fs.cat_file("/data") == expected
    finally:
        file.close()
    assert not dummy_fs._client._fds


def test_scalar_automatic_append_flush_keeps_residual_position(dummy_fs):
    dummy_fs.pipe_file("/data", b"old")
    with writer(dummy_fs, False, 4) as file:
        with dummy_fs.open("/data", "ab") as other:
            other.write(b"external")
        assert file.write(b"123456") == 6
        assert dummy_fs.cat_file("/data") == b"oldexternal1234"
        assert file.tell() == len(b"oldexternal123456")
        file.flush()
        assert file.tell() == len(b"oldexternal123456")
        assert file.truncate() == len(b"oldexternal123456")
    assert dummy_fs.cat_file("/data") == b"oldexternal123456"


def test_short_scalar_appends_reconcile_each_acknowledged_position(
    dummy_fs, monkeypatch
):
    dummy_fs.pipe_file("/data", b"old")
    append = dummy_fs._client.write_positioned

    def short(fd, data):
        return append(fd, data[:2])

    with writer(dummy_fs, False, 100) as file:
        with dummy_fs.open("/data", "ab") as other:
            other.write(b"external")
        monkeypatch.setattr(dummy_fs._client, "write_positioned", short)
        file.write(b"123456")
        file.flush()
        assert file.tell() == len(b"oldexternal123456")
        file.truncate()
    assert dummy_fs.cat_file("/data") == b"oldexternal123456"


def test_group_duplicate_append_handles_retain_individual_positions(dummy_fs):
    dummy_fs.pipe_file("/data", b"old")
    files = dummy_fs.open_many(
        [OpenFile(dummy_fs, "/data", "ab") for _ in range(2)],
        write_buffering=True,
        block_size=2,
    )
    try:
        with dummy_fs.open("/data", "ab") as other:
            other.write(b"external")
        files[0].write(b"AAAA")
        files[1].write(b"BBBB")
        files[0].flush()
        first_end = files[0]._raw.tell()
        assert files[0].tell() == first_end
        files[1].flush()
        assert files[1].tell() == files[1]._raw.tell()
        assert files[1].tell() == len(b"oldexternalAAAABBBB")
    finally:
        for file in files:
            file.close()
    assert dummy_fs.cat_file("/data") == b"oldexternalAAAABBBB"
