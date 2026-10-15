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


def test_native_recursive_copy_creates_nested_share_directories(smb_fs):
    smb_fs.pipe_file("/source/nested/file", b"payload")
    smb_fs.mkdir("/source/nested/empty")
    smb_fs._client.cp_recursive(
        smb_fs._native_path("/source"), smb_fs._native_path("/copy"), False
    )
    assert smb_fs.cat_file("/copy/nested/file") == b"payload"
    assert smb_fs.isdir("/copy/nested/empty")


@pytest.mark.parametrize(
    "operation", ["mkdir", "makedirs", "open", "group", "pipe", "put"]
)
def test_default_directory_creation_uses_share_permissions(smb_fs, tmp_path, operation):
    if operation == "mkdir":
        smb_fs.mkdir("/parent/child")
    elif operation == "makedirs":
        smb_fs.makedirs("/parent/child")
    elif operation == "open":
        with smb_fs.open("/parent/child/file", "wb") as file:
            file.write(b"payload")
    elif operation == "group":
        from fsspec.core import OpenFile, OpenFiles

        entries = [
            OpenFile(smb_fs, path, "wb")
            for path in ("/parent/child/file", "/parent/child/sibling")
        ]
        with OpenFiles(entries, mode="wb", fs=smb_fs) as files:
            for file in files:
                file.write(b"payload")
        assert smb_fs.cat_file("/parent/child/sibling") == b"payload"
    elif operation == "pipe":
        smb_fs.pipe({"/parent/child/file": b"payload"})
    else:
        source = tmp_path / "source"
        source.write_bytes(b"payload")
        smb_fs.put_file(str(source), "/parent/child/file")
    assert smb_fs.isdir("/parent/child")
    if operation not in {"mkdir", "makedirs"}:
        assert smb_fs.cat_file("/parent/child/file") == b"payload"


@pytest.mark.parametrize("mode", [0o700, 0o755])
@pytest.mark.parametrize("create_parents", [False, True])
def test_explicit_unsupported_directory_permissions_are_not_ignored(
    smb_fs, mode, create_parents
):
    with pytest.raises(NotImplementedError):
        smb_fs.mkdir("/private", mode=mode, create_parents=create_parents)
    assert not smb_fs.exists("/private")
    smb_fs.mkdir("/share-default", mode=0o777, create_parents=create_parents)
    assert smb_fs.isdir("/share-default")


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
