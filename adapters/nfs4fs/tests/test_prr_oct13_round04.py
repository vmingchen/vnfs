"""PRR round 4: every copy route honors explicit symlink preservation."""

import errno

import fsspec
import pytest
from fsspec.callbacks import Callback


@pytest.mark.parametrize("route", ["existing", "trailing", "paired", "depth"])
@pytest.mark.parametrize("target", ["file", "missing", "directory"])
@pytest.mark.parametrize("prefix", ["", "namespace"])
def test_copy_fallback_preserves_stored_symlink_target(tmp_path, route, target, prefix):
    remote = tmp_path / "remote"
    (remote / prefix).mkdir(parents=True)
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(remote),
        root=prefix,
        auto_mkdir=True,
        skip_instance_cache=True,
    ) as fs:
        fs.makedirs("/source/directory")
        fs.pipe_file("/source/file", b"payload")
        fs.pipe_file("/source/directory/child", b"child")
        fs.symlink(target, "/source/link")
        stored = fs.readlink("/source/link")
        fs.mkdir("/destination")
        source, destination = "/source", "/destination"
        result = "/destination/source/link"
        options = {}
        if route == "trailing":
            source, destination = source + "/", destination + "/"
            result = "/destination/link"
        elif route == "paired":
            source, destination = ["/source/link"], ["/destination/link"]
            result = "/destination/link"
        elif route == "depth":
            options["maxdepth"] = 1
        callback = Callback()
        fs.copy(
            source,
            destination,
            recursive=True,
            symlinks=True,
            on_error="raise",
            callback=callback,
            **options,
        )
        attrs, errors = fs._client.lstat_many([fs._native_path(result)])
        assert not errors
        assert attrs[0]["type"] == "symlink"
        assert fs.readlink(result) == stored
        assert fs.cat_file("/source/file") == b"payload"
        assert callback.value == callback.size
        assert not fs._client._fds


@pytest.mark.parametrize("route", ["new", "existing"])
@pytest.mark.parametrize("on_error", ["raise", "ignore"])
def test_symlink_copy_creation_failure_honors_error_policy(
    dummy_fs, monkeypatch, route, on_error
):
    dummy_fs.makedirs("/source")
    dummy_fs.pipe_file("/source/file", b"payload")
    dummy_fs.symlink("file", "/source/link")
    destination = "/destination"
    if route == "existing":
        dummy_fs.mkdir(destination)
        destination += "/source"
    failed_path = dummy_fs._native_path(destination + "/link")
    symlink = dummy_fs._client.symlink

    def fail_link(target, path):
        if path == failed_path:
            raise PermissionError(errno.EACCES, "injected link creation failure")
        return symlink(target, path)

    callback = Callback()
    monkeypatch.setattr(dummy_fs._client, "symlink", fail_link)

    def copy():
        dummy_fs.copy(
            "/source",
            "/destination",
            recursive=True,
            symlinks=True,
            on_error=on_error,
            callback=callback,
        )

    if on_error == "raise":
        with pytest.raises(PermissionError, match="link creation"):
            copy()
    else:
        copy()
        assert callback.value == callback.size
    assert dummy_fs.cat_file(destination + "/file") == b"payload"
    attrs, errors = dummy_fs._client.lstat_many([failed_path])
    assert attrs == [None]
    assert errors[0] == errno.ENOENT
    assert dummy_fs.readlink("/source/link") == "file"


@pytest.mark.parametrize("target", ["/source/file", "file", "file-\udcff"])
@pytest.mark.parametrize("prefix", ["", "namespace"])
def test_existing_destination_preserves_absolute_and_non_utf8_targets(
    tmp_path, target, prefix
):
    remote = tmp_path / "remote"
    (remote / prefix).mkdir(parents=True)
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(remote),
        root=prefix,
        auto_mkdir=True,
        skip_instance_cache=True,
    ) as fs:
        fs.mkdir("/source")
        fs.pipe_file("/source/" + target.rsplit("/", 1)[-1], b"payload")
        fs.symlink(target, "/source/link")
        stored = fs.readlink("/source/link")
        fs.mkdir("/destination")
        fs.copy(
            "/source", "/destination", recursive=True, symlinks=True, on_error="raise"
        )
        assert fs.readlink("/destination/source/link") == stored
        assert fs.cat_file("/destination/source/link") == b"payload"


@pytest.mark.parametrize("on_error", ["raise", "ignore"])
def test_ambiguous_link_creation_is_not_suppressed_or_replayed(
    dummy_fs, monkeypatch, on_error
):
    dummy_fs.mkdir("/source")
    dummy_fs.pipe_file("/source/file", b"payload")
    dummy_fs.symlink("file", "/source/link")
    dummy_fs.mkdir("/destination")
    original = dummy_fs._client.symlink
    primary = ConnectionError("injected lost link reply")
    attempted = []

    def apply_then_lose_reply(target, path):
        attempted.append((target, path))
        original(target, path)
        raise primary

    monkeypatch.setattr(dummy_fs._client, "symlink", apply_then_lose_reply)
    with pytest.raises(ConnectionError) as error:
        dummy_fs.copy(
            "/source", "/destination", recursive=True, symlinks=True, on_error=on_error
        )
    assert error.value is primary
    assert attempted == [("file", dummy_fs._native_path("/destination/source/link"))]
    assert dummy_fs.readlink("/destination/source/link") == "file"
    assert dummy_fs.cat_file("/source/file") == b"payload"


def test_default_copy_dereferences_file_links(dummy_fs):
    dummy_fs.mkdir("/source")
    dummy_fs.pipe_file("/source/file", b"payload")
    dummy_fs.symlink("file", "/source/link")
    dummy_fs.mkdir("/destination")
    dummy_fs.copy("/source", "/destination", recursive=True, on_error="raise")
    attrs, errors = dummy_fs._client.lstat_many(["/destination/source/link"])
    assert not errors
    assert attrs[0]["type"] == "file"
    assert dummy_fs.cat_file("/destination/source/link") == b"payload"
