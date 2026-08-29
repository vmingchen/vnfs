"""Unit tests for vnfs_fs on the local-directory (dummy) backend."""

import fsspec
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
    # performs no compounds. Reset first: the counters are process-global, so
    # earlier NFS tests may have left a nonzero total.
    dummy_fs._client.compound_stats()
    stats = dummy_fs._client.compound_stats()
    assert len(stats) == 4
    assert stats == (0, 0, 0, 0)


def test_pipe_missing_parent_raises_by_default(dummy_fs):
    # Aligned with LocalFileSystem(auto_mkdir=False): writing into a missing
    # directory is an error, not an implicit mkdir.
    with pytest.raises(FileNotFoundError):
        dummy_fs.pipe({"nfs4:///no/such/dir/a.txt": b"x"})
    with pytest.raises(FileNotFoundError):
        dummy_fs.touch("nfs4:///no/such/dir/b.txt")


def test_pipe_auto_mkdir_creates_parents(tmp_path):
    fs = fsspec.filesystem(
        "nfs4", backend="dummy", dummy_root=str(tmp_path / "root"), auto_mkdir=True
    )
    fs.pipe({"nfs4:///a/deep/dir/a.txt": b"x"})
    assert fs.cat_file("nfs4:///a/deep/dir/a.txt") == b"x"
    assert fs.isdir("nfs4:///a/deep/dir")


def test_pipe_existing_target_and_parent(dummy_fs):
    dummy_fs.mkdir("nfs4:///dir1", create_parents=True)
    dummy_fs.pipe({"nfs4:///dir1/a.txt": b"old-long-content"})
    # Existing parent: succeeds.
    dummy_fs.pipe({"nfs4:///dir1/b.txt": b"world"})
    assert dummy_fs.cat_file("nfs4:///dir1/b.txt") == b"world"
    # Existing target: overwritten and truncated (wb semantics).
    dummy_fs.pipe({"nfs4:///dir1/a.txt": b"hi"})
    assert dummy_fs.cat_file("nfs4:///dir1/a.txt") == b"hi"
