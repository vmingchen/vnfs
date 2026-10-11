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


@pytest.mark.parametrize("operation", ["scalar", "group", "pipe", "put"])
@pytest.mark.parametrize("existing", [False, True])
def test_smb_transactions_reject_unenforceable_private_staging(
    smb_fs, tmp_path, monkeypatch, operation, existing
):
    from fsspec.core import OpenFile, OpenFiles
    from vfsi_fsspec import _fs as engine

    fs = smb_fs
    fs.pipe_file("/existing", b"original")
    destination = "/existing" if existing else "/missing/new"
    source = tmp_path / "upload"
    source.write_bytes(b"secret")
    allocations = []
    spool = engine.tempfile.SpooledTemporaryFile

    def record_spool(*args, **kwargs):
        allocations.append(True)
        return spool(*args, **kwargs)

    monkeypatch.setattr(engine.tempfile, "SpooledTemporaryFile", record_spool)
    with fs.transaction:
        with pytest.raises(NotImplementedError, match="private.*permission"):
            if operation == "scalar":
                with fs.open(destination, "wb") as file:
                    file.write(b"secret")
            elif operation == "group":
                with OpenFiles([OpenFile(fs, destination, "wb")], mode="wb", fs=fs):
                    pass
            elif operation == "pipe":
                fs.pipe_file(destination, b"secret")
            else:
                fs.put(str(source), destination)
        assert not fs.transaction.files
    assert allocations == []
    assert fs.cat_file("/existing") == b"original"
    assert not fs.exists("/missing")
    assert fs.ls("/", detail=False) == ["vsmbfs:///existing"]
    assert not fs._client._fds


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
