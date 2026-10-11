"""Integration coverage for the Python/fsspec SMB backend."""

import fsspec
import pytest
from vsmb import _native

from .common import run_correctness_suite


def test_correctness_suite_on_smb(smb_fs):
    run_correctness_suite(smb_fs)


def test_smb_identity_and_capabilities(smb_fs):
    assert 0x0202 <= smb_fs.smb_dialect() <= 0x0311
    assert smb_fs._client.minor_version() is None
    assert smb_fs._client.capabilities() & _native.CAP_LSTAT == 0


def test_rmdir_without_lstat_preserves_files_and_nonempty_directories(smb_fs):
    smb_fs.pipe_file("/file", b"keep")
    with pytest.raises(NotADirectoryError):
        smb_fs.rmdir("/file")
    assert smb_fs.cat_file("/file") == b"keep"
    smb_fs.mkdir("/directory")
    smb_fs.pipe_file("/directory/child", b"child")
    with pytest.raises(OSError):
        smb_fs.rmdir("/directory")
    assert smb_fs.cat_file("/directory/child") == b"child"
    smb_fs.mkdir("/empty")
    smb_fs.rmdir("/empty")
    assert not smb_fs.exists("/empty")
    with pytest.raises(FileNotFoundError):
        smb_fs.rmdir("/missing")


def test_persistent_blockcache_observes_smb_overwrite(smb_fs, tmp_path):
    cached = fsspec.filesystem(
        "blockcache",
        fs=smb_fs,
        cache_storage=str(tmp_path / "cache"),
        cache_check=0,
        check_files=False,
        expiry_time=0,
        skip_instance_cache=True,
    )
    try:
        smb_fs.pipe_file("/file", b"old!")
        with cached.open("/file", "rb", block_size=4) as file:
            assert file.read() == b"old!"
        smb_fs.pipe_file("/file", b"new!")
        with cached.open("/file", "rb", block_size=4) as file:
            assert file.read() == b"new!"
    finally:
        cached.clear_cache()
