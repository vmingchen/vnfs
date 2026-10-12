"""Production-review regressions for nontruncating touch."""

import time

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("kind", ["file", "symlink"])
@pytest.mark.parametrize("auto_mkdir", [False, True])
def test_nontruncating_touch_preserves_concurrent_creator(
    fs, monkeypatch, kind, auto_mkdir
):
    fs.auto_mkdir = auto_mkdir
    fs.pipe_file("/winner", b"concurrent contents")
    previous_mtime = fs.info("/winner")["modified_ns"]
    time.sleep(0.02)
    original_exists = fs.exists
    original_touch = fs._client.touch
    installed = []

    def install():
        if installed:
            return
        installed.append(True)
        if kind == "file":
            fs.hardlink("/winner", "/target")
        else:
            fs.symlink("winner", "/target")

    def raced_exists(path, **kwargs):
        result = original_exists(path, **kwargs)
        if path == "/target" and not result:
            install()
        return result

    def raced_touch(path):
        try:
            return original_touch(path)
        except FileNotFoundError:
            if path == fs._native_path("/target"):
                install()
            raise

    monkeypatch.setattr(fs, "exists", raced_exists)
    monkeypatch.setattr(fs._client, "touch", raced_touch)
    fs.touch("/target", truncate=False)
    assert installed
    assert fs.info("/target")["modified_ns"] != previous_mtime
    assert fs.cat_file("/target") == b"concurrent contents"
    assert fs.cat_file("/winner") == b"concurrent contents"
    if kind == "symlink":
        assert fs.readlink("/target") == "winner"
    assert not fs._client._fds


def test_transactional_nontruncating_touch_does_not_replace_concurrent_file(fs):
    with pytest.raises(FileExistsError):
        with fs.transaction:
            fs.touch("/target", truncate=False)
            assert not fs.exists("/target")
            # Bypass transaction staging to model a concurrent creator.
            fs._client.write_many(
                [fs._native_path("/target")], [b"concurrent contents"]
            )
    assert fs.cat_file("/target") == b"concurrent contents"
    assert [fs._strip_protocol(entry["name"]) for entry in fs.ls("/")] == ["/target"]
    assert not fs._client._fds


@pytest.mark.parametrize("auto_mkdir", [False, True])
@pytest.mark.parametrize("symlink", [False, True])
def test_nontruncating_touch_creates_missing_file(fs, auto_mkdir, symlink):
    fs.auto_mkdir = auto_mkdir
    if not auto_mkdir:
        fs.mkdir("/parent")
    path = "/parent/target"
    if symlink:
        fs.symlink("parent/target", "/link")
        path = "/link"
        if auto_mkdir:
            # auto_mkdir prepares named parents, not dangling symlink targets.
            fs.mkdir("/parent")
    fs.touch(path, truncate=False)
    assert fs.cat_file("/parent/target") == b""
    if symlink:
        assert fs.readlink("/link") == "parent/target"
    assert not fs._client._fds


@pytest.mark.parametrize("commit", [False, True])
def test_transactional_nontruncating_touch_defers_creation(fs, commit):
    fs.start_transaction()
    try:
        fs.touch("/target", truncate=False)
        assert not fs.exists("/target")
    finally:
        fs.transaction.complete(commit=commit)
    assert fs.exists("/target") is commit
    if commit:
        assert fs.cat_file("/target") == b""
    assert not fs._client._fds


@pytest.mark.parametrize("error_type", [PermissionError, ConnectionError])
def test_nontruncating_touch_propagates_other_failures(fs, monkeypatch, error_type):
    error = error_type("touch failed")

    def fail(path):
        raise error

    monkeypatch.setattr(fs._client, "touch", fail)
    with pytest.raises(error_type) as caught:
        fs.touch("/target", truncate=False)
    assert caught.value is error
    assert not fs.exists("/target")


@pytest.mark.parametrize("pending", ["touch", "wb", "xb", "ab"])
def test_transactional_touch_preserves_pending_write(fs, pending):
    with fs.transaction:
        if pending == "touch":
            fs.touch("/target", truncate=False)
            expected = b""
        else:
            with fs.open("/target", pending) as file:
                file.write(b"pending contents")
            expected = b"pending contents"
        fs.touch("nfs4:///./target", truncate=False)
        assert not fs.exists("/target")
    assert fs.cat_file("/target") == expected
    assert [fs._strip_protocol(entry["name"]) for entry in fs.ls("/")] == ["/target"]
    assert not fs._client._fds
