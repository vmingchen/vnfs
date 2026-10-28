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
