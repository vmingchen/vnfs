"""PRR round 2: mixed grouped opens only create writable parents."""

import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("read_mode", ["rb", "rb+"])
@pytest.mark.parametrize("write_mode", ["wb", "ab", "xb"])
@pytest.mark.parametrize("reader_first", [False, True])
def test_failed_mixed_open_does_not_create_read_or_update_parent(
    dummy_fs, read_mode, write_mode, reader_first
):
    dummy_fs.auto_mkdir = True
    entries = [
        OpenFile(dummy_fs, "/read-parent/input", read_mode),
        OpenFile(dummy_fs, "/write-parent/output", write_mode),
    ]
    if not reader_first:
        entries.reverse()
    with pytest.raises(FileNotFoundError):
        dummy_fs.open_many(entries)
    assert not dummy_fs.exists("/read-parent")
    assert dummy_fs.isdir("/write-parent")
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("read_mode", ["rb", "rb+"])
@pytest.mark.parametrize("write_mode", ["wb", "ab", "xb"])
def test_valid_mixed_open_retains_reads_and_creates_writer_parent(
    dummy_fs, read_mode, write_mode
):
    dummy_fs.auto_mkdir = True
    dummy_fs.pipe_file("/read-parent/input", b"input data")
    files = dummy_fs.open_many(
        [
            OpenFile(dummy_fs, "/read-parent/input", read_mode),
            OpenFile(dummy_fs, "/write-parent/output", write_mode),
        ]
    )
    try:
        assert files[0].read() == b"input data"
        assert files[1].write(b"output data") == 11
    finally:
        dummy_fs.commit_many(files)
    assert dummy_fs.cat_file("/read-parent/input") == b"input data"
    assert dummy_fs.cat_file("/write-parent/output") == b"output data"
    assert not dummy_fs._client._fds
