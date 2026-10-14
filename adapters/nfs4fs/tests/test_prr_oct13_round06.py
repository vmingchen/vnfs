"""PRR round 6: native operational errors retain errno and exact path identity."""

import errno
import os

import pytest
from fsspec.callbacks import Callback


@pytest.mark.parametrize("name", ["ordinary", "ordinary-\udcff"])
def test_readlink_on_regular_file_is_operational_error(dummy_fs, name):
    path = "/" + name
    dummy_fs.pipe_file(path, b"payload")
    with pytest.raises(OSError) as error:
        dummy_fs.readlink(path)
    assert error.value.errno == errno.EINVAL
    assert os.fsencode(error.value.filename) == os.fsencode(dummy_fs._native_path(path))


@pytest.mark.parametrize("on_error", ["ignore", "raise"])
def test_symlink_replacement_race_obeys_copy_error_policy(
    dummy_fs, monkeypatch, on_error
):
    dummy_fs.pipe_file("/source", b"payload")
    dummy_fs.symlink("source", "/link")
    readlink = dummy_fs._client.readlink
    replaced = []

    def replace_then_read(path):
        if not replaced:
            replaced.append(True)
            dummy_fs.rm_file("/link")
            dummy_fs.pipe_file("/link", b"replacement")
        return readlink(path)

    monkeypatch.setattr(dummy_fs._client, "readlink", replace_then_read)
    callback = Callback()

    def copy():
        dummy_fs.copy(
            ["/link"],
            ["/destination"],
            symlinks=True,
            on_error=on_error,
            callback=callback,
        )

    if on_error == "raise":
        with pytest.raises(OSError) as error:
            copy()
        assert error.value.errno == errno.EINVAL
    else:
        copy()
        assert callback.size == callback.value == 1
    assert not dummy_fs.exists("/destination")
    assert dummy_fs.cat_file("/link") == b"replacement"
    assert dummy_fs.cat_file("/source") == b"payload"


@pytest.mark.parametrize("name", ["missing", "missing-\udcff", "missing-\udcfe"])
@pytest.mark.parametrize("operation", ["open", "listdir", "mkdir_parent", "exclusive"])
def test_native_errno_errors_preserve_exact_filename(dummy_fs, name, operation):
    path = "/" + name
    if operation == "exclusive":
        dummy_fs.pipe_file(path, b"existing")
    elif operation == "mkdir_parent":
        path += "/child"
    with pytest.raises(OSError) as error:
        if operation == "open":
            dummy_fs.open(path, "rb")
        elif operation == "listdir":
            dummy_fs.ls(path)
        elif operation == "mkdir_parent":
            dummy_fs.mkdir(path, create_parents=False)
        else:
            dummy_fs.open(path, "xb")
    assert error.value.errno == (
        errno.EEXIST if operation == "exclusive" else errno.ENOENT
    )
    assert os.fsencode(error.value.filename) == os.fsencode(dummy_fs._native_path(path))
    assert not dummy_fs._client._fds


def test_native_batched_error_keeps_index_and_filename(dummy_fs):
    path = "/missing-\udcff"
    with pytest.raises(FileNotFoundError) as error:
        dummy_fs._client.open_many([dummy_fs._native_path(path)], ["rb"])
    assert error.value.index == 0
    assert error.value.errno == errno.ENOENT
    assert os.fsencode(error.value.filename) == os.fsencode(dummy_fs._native_path(path))


@pytest.mark.parametrize(
    "operation", ["invalid_parent", "directory_write", "permission"]
)
def test_native_error_subclasses_keep_errno_and_filename(dummy_fs, operation):
    dummy_fs.pipe_file("/file", b"payload")
    dummy_fs.mkdir("/directory")
    path = "/file/child" if operation == "invalid_parent" else "/directory"
    exception, code = NotADirectoryError, errno.ENOTDIR
    if operation == "directory_write":
        exception, code = IsADirectoryError, errno.EISDIR
    elif operation == "permission":
        if os.geteuid() == 0:
            pytest.skip("root bypasses file permissions")
        path = "/file"
        exception, code = PermissionError, errno.EACCES
        dummy_fs._client.chmod(dummy_fs._native_path(path), 0)
    try:
        with pytest.raises(exception) as error:
            if operation == "invalid_parent":
                dummy_fs.mkdir(path, create_parents=False)
            else:
                dummy_fs.open(path, "wb" if operation == "directory_write" else "rb")
        assert error.value.errno == code
        assert error.value.filename == dummy_fs._native_path(path)
    finally:
        if operation == "permission":
            dummy_fs._client.chmod(dummy_fs._native_path(path), 0o600)
    assert not dummy_fs._client._fds
