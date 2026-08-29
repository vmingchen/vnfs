"""Unit tests for vnfs_fs on the local-directory (dummy) backend."""

import pytest

from .common import run_correctness_suite


def test_correctness_suite(dummy_fs):
    run_correctness_suite(dummy_fs)


def test_protocol_registered():
    from fsspec.registry import registry

    assert "nfs4" in registry


def test_dummy_root_is_isolated(dummy_fs, tmp_path):
    # Writes stay inside the configured dummy root.
    dummy_fs.pipe_file("nfs4:///only-here.txt", b"x")
    assert (tmp_path / "root" / "only-here.txt").read_bytes() == b"x"
    assert not (tmp_path / "only-here.txt").exists()


def test_lazy_read_open_does_not_open_until_io(dummy_fs):
    dummy_fs.pipe_file("nfs4:///f.txt", b"data")
    f = dummy_fs.open("nfs4:///f.txt", "rb")
    assert f._fd is None  # lazy
    assert f.read() == b"data"
    assert f._fd is not None
    f.close()


def test_write_open_truncates_eagerly(dummy_fs):
    dummy_fs.pipe_file("nfs4:///f.txt", b"long-content")
    f = dummy_fs.open("nfs4:///f.txt", "wb")
    assert f._fd is not None  # eager
    assert dummy_fs.size("nfs4:///f.txt") == 0  # truncated at open
    f.close()


def test_error_mapping(dummy_fs):
    with pytest.raises(FileNotFoundError):
        dummy_fs.info("nfs4:///no/such/path")
    with pytest.raises(FileNotFoundError):
        dummy_fs.cat_file("nfs4:///no/such/path")
    with pytest.raises(FileNotFoundError):
        dummy_fs.cat("nfs4:///no/such/path")
    with pytest.raises(FileNotFoundError):
        dummy_fs.open("nfs4:///no/such/path", "rb").read()
    with pytest.raises(IsADirectoryError):
        dummy_fs.cat_file("nfs4:///")


def test_compound_stats_available(dummy_fs):
    # The counter exists and is reset-on-read even though the dummy backend
    # performs no compounds.
    stats = dummy_fs._client.compound_stats()
    assert len(stats) == 4
    assert stats == dummy_fs._client.compound_stats()
