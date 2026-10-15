"""Directory moves reject existing child entries before native rename."""

import errno
import shutil

import pytest


@pytest.mark.parametrize("entry", ["file", "directory", "symlink", "dangling"])
def test_move_into_directory_preserves_colliding_child(dummy_fs, monkeypatch, entry):
    dummy_fs.pipe_file("/source", b"new")
    dummy_fs.mkdir("/dest")
    if entry == "file":
        dummy_fs.pipe_file("/dest/source", b"old")
    elif entry == "directory":
        dummy_fs.mkdir("/dest/source")
    else:
        dummy_fs.pipe_file("/target", b"old")
        dummy_fs.symlink(
            "/target" if entry == "symlink" else "/missing", "/dest/source"
        )
    calls = []
    rename = dummy_fs._client.rename_many

    def capture(*args):
        calls.append(args)
        return rename(*args)

    monkeypatch.setattr(dummy_fs._client, "rename_many", capture)
    with pytest.raises(shutil.Error):
        dummy_fs.mv("/source", "/dest")
    assert not calls
    assert dummy_fs.cat_file("/source") == b"new"
    if entry == "file":
        assert dummy_fs.cat_file("/dest/source") == b"old"
    elif entry == "directory":
        assert dummy_fs.isdir("/dest/source")
    else:
        assert dummy_fs.readlink("/dest/source") == (
            "/target" if entry == "symlink" else "/missing"
        )


def test_move_directory_into_directory_does_not_replace_empty_child(dummy_fs):
    dummy_fs.mkdir("/source")
    dummy_fs.pipe_file("/source/data", b"new")
    dummy_fs.makedirs("/dest/source")
    with pytest.raises(shutil.Error):
        dummy_fs.mv("/source", "/dest", recursive=True)
    assert dummy_fs.cat_file("/source/data") == b"new"
    assert dummy_fs.ls("/dest/source") == []


def test_move_child_probe_errors_do_not_rename(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/source", b"new")
    dummy_fs.mkdir("/dest")
    calls = []

    def denied(paths):
        assert paths == ["/dest/source"]
        return [None], {0: errno.EACCES}

    monkeypatch.setattr(dummy_fs._client, "lstat_many", denied)
    monkeypatch.setattr(
        dummy_fs._client, "rename_many", lambda *args: calls.append(args)
    )
    with pytest.raises(PermissionError) as error:
        dummy_fs.mv("/source", "/dest")
    assert error.value.filename == "/dest/source"
    assert not calls
    assert dummy_fs.cat_file("/source") == b"new"


@pytest.mark.parametrize("paired", [False, True])
def test_explicit_file_destinations_keep_replace_semantics(dummy_fs, paired):
    dummy_fs.pipe_file("/source", b"new")
    dummy_fs.pipe_file("/dest", b"old")
    dummy_fs.mv(["/source"] if paired else "/source", ["/dest"] if paired else "/dest")
    assert dummy_fs.cat_file("/dest") == b"new"
    assert not dummy_fs.exists("/source")


def test_directory_move_with_missing_child_still_succeeds(dummy_fs):
    dummy_fs.pipe_file("/source", b"new")
    dummy_fs.mkdir("/dest")
    dummy_fs.mv("/source", "/dest")
    assert dummy_fs.cat_file("/dest/source") == b"new"
    assert not dummy_fs.exists("/source")
