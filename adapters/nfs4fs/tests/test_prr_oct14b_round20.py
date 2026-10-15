"""Single-file uploads share complete-write and exclusive-create guarantees."""

import errno

import pytest
from fsspec.callbacks import Callback


@pytest.mark.parametrize("accepted", [1, 2, 3])
def test_put_file_completes_short_writes(dummy_fs, tmp_path, monkeypatch, accepted):
    source = tmp_path / "source"
    source.write_bytes(b"abcdefgh")
    pwrite = dummy_fs._client.pwrite
    calls = []

    def short(fd, data, offset):
        calls.append(offset)
        return pwrite(fd, data[:accepted], offset)

    monkeypatch.setattr(dummy_fs._client, "pwrite", short)
    callback = Callback()
    dummy_fs.put_file(str(source), "/parent/target", callback=callback)
    assert dummy_fs.cat_file("/parent/target") == b"abcdefgh"
    assert callback.value == callback.size == 8
    assert len(calls) >= 3
    assert not dummy_fs._client._fds


def test_put_file_zero_progress_is_an_error(dummy_fs, tmp_path, monkeypatch):
    source = tmp_path / "source"
    source.write_bytes(b"abcdefgh")
    monkeypatch.setattr(dummy_fs._client, "pwrite", lambda *args: 0)
    with pytest.raises(OSError) as error:
        dummy_fs.put_file(str(source), "/target")
    assert error.value.errno == errno.EIO
    assert not dummy_fs._client._fds


def test_put_file_create_race_preserves_concurrent_winner(
    dummy_fs, tmp_path, monkeypatch
):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    original_open = dummy_fs.open
    raced = False

    def concurrent_create(path, mode="rb", **kwargs):
        nonlocal raced
        if path == "/target" and not raced:
            raced = True
            dummy_fs.pipe_file("/target", b"winner")
        return original_open(path, mode, **kwargs)

    monkeypatch.setattr(dummy_fs, "open", concurrent_create)
    with pytest.raises(FileExistsError):
        dummy_fs.put_file(str(source), "/target", mode="create")
    assert raced
    assert dummy_fs.cat_file("/target") == b"winner"
    assert not dummy_fs._client._fds


def test_put_file_invalid_mode_never_overwrites(dummy_fs, tmp_path):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    dummy_fs.pipe_file("/target", b"original")
    with pytest.raises(ValueError):
        dummy_fs.put_file(str(source), "/target", mode="invalid")
    assert dummy_fs.cat_file("/target") == b"original"


@pytest.mark.parametrize("mode", ["overwrite", "create"])
def test_put_file_success_and_empty_file_controls(dummy_fs, tmp_path, mode):
    source = tmp_path / "source"
    source.write_bytes(b"")
    callback = Callback()
    dummy_fs.put_file(str(source), "/parent/target", mode=mode, callback=callback)
    assert dummy_fs.cat_file("/parent/target") == b""
    assert callback.value == callback.size == 0
    assert not dummy_fs._client._fds


def test_put_file_missing_source_preserves_remote_destination(dummy_fs, tmp_path):
    dummy_fs.pipe_file("/target", b"original")
    with pytest.raises(FileNotFoundError):
        dummy_fs.put_file(str(tmp_path / "missing"), "/target")
    assert dummy_fs.cat_file("/target") == b"original"


def test_put_file_directory_remains_a_directory_operation(dummy_fs, tmp_path):
    source = tmp_path / "directory"
    source.mkdir()
    dummy_fs.put_file(str(source), "/parent/directory")
    assert dummy_fs.isdir("/parent/directory")
    with pytest.raises(FileExistsError):
        dummy_fs.put_file(str(source), "/parent/directory", mode="create")


@pytest.mark.parametrize("commit", [False, True])
def test_put_file_respects_transaction_publication(dummy_fs, tmp_path, commit):
    source = tmp_path / "source"
    source.write_bytes(b"upload")
    dummy_fs.pipe_file("/target", b"original")
    transaction = dummy_fs.start_transaction()
    dummy_fs.put_file(str(source), "/target")
    assert dummy_fs.cat_file("/target") == b"original"
    transaction.complete(commit=commit)
    assert dummy_fs.cat_file("/target") == (b"upload" if commit else b"original")
    assert not dummy_fs._client._fds
