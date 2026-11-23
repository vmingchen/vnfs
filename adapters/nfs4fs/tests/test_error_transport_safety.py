"""Precise native exception classes must preserve transport replay safety."""

import errno

import pytest


def failure(kind):
    code = {
        "timeout": errno.ETIMEDOUT,
        "auth": errno.EACCES,
        "invalid_reply": errno.EBADMSG,
    }[kind]
    error = OSError(code, "injected transport failure")
    error.transport_kind = kind
    return error


@pytest.mark.parametrize("kind", ["timeout", "auth", "invalid_reply"])
@pytest.mark.parametrize("operation", ["write", "truncate"])
def test_raw_mutation_transport_failure_poisoning_never_replays(
    dummy_fs, monkeypatch, kind, operation
):
    file = dummy_fs.open("/file", "wb", cache_type="none", write_buffering=False)
    calls = []
    error = failure(kind)

    def fail(*args):
        calls.append(args)
        raise error

    monkeypatch.setattr(
        dummy_fs._client, "pwrite" if operation == "write" else "ftruncate", fail
    )
    monkeypatch.setattr(dummy_fs._client, "reconnect_descriptor", lambda fd: None)
    with pytest.raises(type(error)) as caught:
        file.write(b"data") if operation == "write" else file.truncate(1)
    assert caught.value is error
    with pytest.raises(ConnectionError, match="unusable"):
        file.write(b"must not replay")
    assert len(calls) == 1
    file.close()


@pytest.mark.parametrize("kind", ["auth", "invalid_reply"])
def test_read_does_not_reconnect_for_authentication_or_malformed_replies(
    dummy_fs, monkeypatch, kind
):
    dummy_fs.pipe_file("/file", b"data")
    file = dummy_fs.open("/file", "rb", cache_type="none")
    calls = []
    error = failure(kind)

    def fail(*args):
        calls.append(args)
        raise error

    monkeypatch.setattr(dummy_fs._client, "pread", fail)
    monkeypatch.setattr(
        dummy_fs._client,
        "reconnect_descriptor",
        lambda fd: pytest.fail("must not reconnect"),
    )
    with pytest.raises(type(error)) as caught:
        file.read(1)
    assert caught.value is error
    assert len(calls) == 1
    file.close()


def uncached_nfs_reply():
    error = OSError(errno.EIO, "NFS replay reply is unavailable")
    error.status_domain = "nfs"
    error.status_code = 10068
    return error


@pytest.mark.parametrize("operation", ["write", "append", "truncate"])
def test_uncached_mutation_reply_retires_unbuffered_writer(
    dummy_fs, monkeypatch, operation
):
    fs = dummy_fs
    fs.pipe_file("/file", b"original")
    mode = "ab" if operation == "append" else "rb+"
    method = {
        "write": "pwrite",
        "append": "write_positioned",
        "truncate": "ftruncate",
    }[operation]
    mutate = getattr(fs._client, method)
    error = uncached_nfs_reply()
    calls = []

    def apply_then_fail(*args):
        calls.append(args)
        mutate(*args)
        raise error

    monkeypatch.setattr(fs._client, method, apply_then_fail)
    monkeypatch.setattr(
        fs._client,
        "reconnect_descriptor",
        lambda fd: pytest.fail(
            "an unavailable cached reply is not a reconnect trigger"
        ),
    )
    with fs.open("/file", mode, cache_type="none", write_buffering=False) as file:
        with pytest.raises(OSError) as caught:
            if operation == "truncate":
                file.truncate(3)
            else:
                file.write(b"new")
        assert caught.value is error
        with pytest.raises(ConnectionError, match="unusable"):
            file.write(b"must not replay")
        assert len(calls) == 1
    assert not fs._client._fds
    assert (
        fs.cat_file("/file")
        == {
            "write": b"newginal",
            "append": b"originalnew",
            "truncate": b"ori",
        }[operation]
    )


@pytest.mark.parametrize("symlink", [False, True])
def test_copy_ignore_keeps_uncertain_nfs_reply(dummy_fs, monkeypatch, symlink):
    fs = dummy_fs
    fs.pipe_file("/target", b"source")
    error = uncached_nfs_reply()
    if symlink:
        fs.symlink("target", "/source")
        native_copy = fs._client.symlink

        def apply_then_fail(target, destination):
            native_copy(target, destination)
            raise error

        monkeypatch.setattr(fs._client, "symlink", apply_then_fail)
    else:
        fs.pipe_file("/source", b"source")
        native_copy = fs._client.copy_many

        def apply_then_fail(pairs):
            copied, errors = native_copy(pairs)
            assert copied[-1] == len(b"source")
            copied[-1] = None
            errors[len(copied) - 1] = error
            return copied, errors

        monkeypatch.setattr(fs._client, "copy_many", apply_then_fail)
    with pytest.raises(OSError) as caught:
        fs.copy(
            ["/missing", "/source"],
            ["/missing-output", "/destination"],
            on_error="ignore",
            symlinks=symlink,
        )
    assert caught.value is error
    assert caught.value.index == 1
    if symlink:
        assert fs.readlink("/destination") == "target"
    else:
        assert fs.cat_file("/destination") == b"source"


def test_remove_preserves_uncertain_later_reply_over_missing_metadata(
    dummy_fs, monkeypatch
):
    fs = dummy_fs
    fs.pipe_file("/file", b"original")
    native_remove = fs._client.remove_many
    error = uncached_nfs_reply()
    error.index = 0
    calls = []

    def apply_then_fail(paths):
        calls.extend(paths)
        native_remove(paths)
        raise error

    monkeypatch.setattr(fs._client, "remove_many", apply_then_fail)
    with pytest.raises(OSError) as caught:
        fs.rm(["/missing", "/file"])
    assert caught.value is error
    assert caught.value.index == 1
    assert caught.value.filename == "/file"
    assert calls == [fs._native_path("/file")]
    assert not fs.exists("/file")


def test_uncertain_copy_reply_stops_missing_parent_recovery(dummy_fs, monkeypatch):
    fs = dummy_fs
    fs.auto_mkdir = True
    fs.pipe({"/one": b"one", "/two": b"two"})
    native_copy = fs._client.copy_many
    error = uncached_nfs_reply()
    batches = []

    def mixed_replies(pairs):
        batches.append(pairs)
        copied, errors = native_copy(pairs)
        if len(batches) == 1:
            assert copied[0] == 3
            assert errors[1].errno == errno.ENOENT
            copied[0] = None
            errors[0] = error
        return copied, errors

    monkeypatch.setattr(fs._client, "copy_many", mixed_replies)
    with pytest.raises(OSError) as caught:
        fs.copy(["/one", "/two"], ["/first", "/new-parent/second"], on_error="ignore")
    assert caught.value is error
    assert caught.value.index == 0
    assert len(batches) == 1
    assert fs.cat_file("/first") == b"one"
    assert not fs.exists("/new-parent")
