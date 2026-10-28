"""Public error semantics survive structured native failures and batching."""

import errno
import os

import fsspec
import pytest
from fsspec.core import OpenFile, OpenFiles


@pytest.mark.parametrize("exist_ok", [False, True])
def test_makedirs_dangling_symlink_matches_local(dummy_fs, tmp_path, exist_ok):
    local = fsspec.filesystem("file")
    local_path = str(tmp_path / "dangling")
    local.symlink(str(tmp_path / "missing"), local_path)
    dummy_fs.symlink("missing", "/dangling")
    with pytest.raises(FileExistsError) as expected:
        local.makedirs(local_path, exist_ok=exist_ok)
    with pytest.raises(type(expected.value)) as actual:
        dummy_fs.makedirs("/dangling", exist_ok=exist_ok)
    assert actual.value.errno == expected.value.errno == errno.EEXIST
    assert dummy_fs.readlink("/dangling") == "missing"


@pytest.mark.parametrize("method", ["cat", "cat_ranges"])
@pytest.mark.parametrize("batch_size", [1, 16])
@pytest.mark.parametrize("missing_first", [False, True])
def test_bulk_read_error_index_uses_public_input_position(
    dummy_fs, method, batch_size, missing_first
):
    dummy_fs.batch_size = batch_size
    dummy_fs.pipe_file("/ok", b"data")
    dummy_fs.mkdir("/directory")
    paths = ["/missing" if missing_first else "/ok", "/directory"]
    if method == "cat":
        results = dummy_fs.cat(paths, on_error="return")
        value, error = results[paths[0]], results[paths[1]]
        assert (
            isinstance(value, FileNotFoundError) if missing_first else value == b"data"
        )
    else:
        value, error = dummy_fs.cat_ranges(
            paths, [0, 0], [None, None], on_error="return"
        )
        assert (
            isinstance(value, FileNotFoundError) if missing_first else value == b"data"
        )
    assert isinstance(error, IsADirectoryError)
    assert error.errno == errno.EISDIR
    assert error.filename == "/directory"
    assert error.index == 1


def test_copy_retry_rebases_filtered_file_indices_without_replaying_success(
    dummy_fs, monkeypatch
):
    dummy_fs.auto_mkdir = True
    dummy_fs.mkdir("/directory")
    dummy_fs.pipe({"/ok": b"data", "/retry": b"data"})
    calls = []
    missing = FileNotFoundError(errno.ENOENT, "missing parent")
    missing.index = 1
    denied = PermissionError(errno.EACCES, "server policy")
    denied.index = 0
    denied.status_domain = "nfs"
    denied.status_code = 13

    def copy_many(pairs):
        calls.append(pairs)
        if len(calls) == 1:
            return [4, None], {1: missing}
        return [None], {0: denied}

    monkeypatch.setattr(dummy_fs._client, "copy_many", copy_many)
    with pytest.raises(PermissionError) as caught:
        dummy_fs.copy(
            ["/directory", "/ok", "/retry"],
            ["/dir-copy", "/ok-copy", "/parents/retry"],
            symlinks=True,
        )
    assert caught.value is denied
    assert denied.index == 2
    assert (denied.status_domain, denied.status_code) == ("nfs", 13)
    assert calls == [
        [("/ok", "/ok-copy"), ("/retry", "/parents/retry")],
        [("/retry", "/parents/retry")],
    ]


def test_copy_error_rebases_after_special_source_filter(
    dummy_fs, tmp_path, monkeypatch
):
    os.mkfifo(tmp_path / "root" / "fifo")
    dummy_fs.pipe_file("/ok", b"data")
    denied = PermissionError(errno.EACCES, "server policy")
    denied.index = 0
    calls = []

    def copy_many(pairs):
        calls.append(pairs)
        return [None], {0: denied}

    monkeypatch.setattr(dummy_fs._client, "copy_many", copy_many)
    with pytest.raises(PermissionError) as caught:
        dummy_fs.copy(["/fifo", "/ok"], ["/fifo-copy", "/ok-copy"])
    assert caught.value is denied
    assert denied.index == 1
    assert calls == [[("/ok", "/ok-copy")]]


@pytest.mark.parametrize("duplicate", [False, True])
def test_group_read_error_uses_openfiles_position(dummy_fs, monkeypatch, duplicate):
    paths = ["/first", "/first" if duplicate else "/second"]
    dummy_fs.pipe({path: b"data" for path in paths})
    error = PermissionError(errno.EACCES, "injected read failure")
    error.index = 0

    def fail_current(fds, offsets, lengths):
        return [None] * len(fds), {0: error}

    with OpenFiles([OpenFile(dummy_fs, path) for path in paths], fs=dummy_fs) as files:
        monkeypatch.setattr(dummy_fs._client, "pread_many", fail_current)
        monkeypatch.setattr(
            dummy_fs,
            "info",
            lambda *args, **kwargs: pytest.fail(
                "error attribution must not fetch metadata"
            ),
        )
        with pytest.raises(PermissionError) as caught:
            files[1].read(1)
        assert caught.value is error
        assert error.index == 1
    assert not dummy_fs._client._fds
