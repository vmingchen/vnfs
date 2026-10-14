"""PRR round 8: directory copies reject non-directory destinations."""

import pytest


def test_cp_file_directory_rejects_existing_file_destination(dummy_fs):
    fs = dummy_fs
    fs.makedirs("/source")
    fs.pipe_file("/destination", b"keep")

    with pytest.raises(FileExistsError):
        fs.cp_file("/source", "/destination")

    assert fs.cat_file("/destination") == b"keep"


def test_recursive_empty_directory_copy_rejects_existing_file_destination(dummy_fs):
    fs = dummy_fs
    fs.makedirs("/empty-source")
    fs.pipe_file("/destination", b"keep")

    with pytest.raises(FileExistsError):
        fs.copy("/empty-source", "/destination", recursive=True)

    assert fs.cat_file("/destination") == b"keep"
