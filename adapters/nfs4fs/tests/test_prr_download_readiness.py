"""Regression coverage for streamed download setup and ownership."""

import errno

import pytest


@pytest.mark.parametrize("code", [errno.ENOENT, errno.EACCES, errno.ECONNRESET])
@pytest.mark.parametrize("paired_paths", [False, True])
def test_streamed_download_source_open_failure_preserves_destination(
    dummy_fs, tmp_path, monkeypatch, code, paired_paths
):
    dummy_fs.pipe_file("/source", b"remote payload")
    dummy_fs.max_batch_bytes = 4
    target = tmp_path / "destination"
    target.write_bytes(b"original local contents")
    native_open = dummy_fs._client.open
    source = dummy_fs._native_path("/source")
    failure = OSError(code, "injected source open failure", source)

    def fail_source_open(path, mode, **kwargs):
        if path == source and mode == "rb":
            raise failure
        return native_open(path, mode, **kwargs)

    monkeypatch.setattr(dummy_fs._client, "open", fail_source_open)
    remote = ["/source"] if paired_paths else "/source"
    local = [str(target)] if paired_paths else str(target)
    with pytest.raises(OSError) as error:
        dummy_fs.get(remote, local)
    assert error.value is failure
    assert target.read_bytes() == b"original local contents"
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("paired_paths", [False, True])
@pytest.mark.parametrize("close_error_type", [OSError, KeyboardInterrupt])
@pytest.mark.parametrize("failure_at", ["local_open", "read", "callback", "success"])
def test_streamed_download_failed_close_preserves_primary_and_defers_cleanup(
    dummy_fs, tmp_path, monkeypatch, paired_paths, close_error_type, failure_at
):
    import builtins
    from fsspec.callbacks import Callback

    payload = b"remote payload"
    dummy_fs.pipe_file("/source", payload)
    dummy_fs.max_batch_bytes = 4
    target = tmp_path / "destination"
    target.write_bytes(b"original local contents")
    primary = (
        PermissionError(errno.EACCES, "local OPEN failed", str(target))
        if failure_at == "local_open"
        else (
            RuntimeError("callback failed")
            if failure_at == "callback"
            else OSError(errno.EIO, "READ failed")
        )
    )
    cleanup = close_error_type("remote CLOSE failed")
    attempted = []
    local_open = builtins.open

    def fail_local_open(path, *args, **kwargs):
        if path == str(target):
            raise primary
        return local_open(path, *args, **kwargs)

    def fail_close(fd):
        attempted.append(fd)
        raise cleanup

    def fail_read(*args, **kwargs):
        raise primary

    class FailingCallback(Callback):
        def branch(self, path1, path2, kwargs):
            kwargs["callback"] = self

        def relative_update(self, inc=1):
            raise primary

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close", fail_close)
        if failure_at == "local_open":
            patch.setattr(builtins, "open", fail_local_open)
        elif failure_at == "read":
            patch.setattr(dummy_fs._client, "pread", fail_read)
        kwargs = {"callback": FailingCallback()} if failure_at == "callback" else {}
        expected = cleanup if failure_at == "success" else primary
        remote = ["/source"] if paired_paths else "/source"
        local = [str(target)] if paired_paths else str(target)
        with pytest.raises(type(expected)) as error:
            dummy_fs.get(remote, local, **kwargs)
        assert error.value is expected
        if failure_at != "success":
            assert error.value.__cause__ is cleanup
        assert dummy_fs._client._fds
        assert (
            set(dummy_fs._client._fds)
            == dummy_fs._client._deferred_close
            == set(attempted)
        )
    if failure_at == "local_open":
        assert target.read_bytes() == b"original local contents"
    elif failure_at == "success":
        assert target.read_bytes() == payload
    # A later path call retries cleanup; no reader is exposed by get().
    assert dummy_fs.info("/source")["size"] == len(payload)
    assert not dummy_fs._client._fds
    assert not dummy_fs._client._deferred_close


def test_streamed_download_local_open_failure_closes_source(
    dummy_fs, tmp_path, monkeypatch
):
    import builtins

    dummy_fs.pipe_file("/source", b"remote payload")
    dummy_fs.max_batch_bytes = 4
    target = tmp_path / "destination"
    target.write_bytes(b"original local contents")
    local_open = builtins.open
    failure = PermissionError(errno.EACCES, "injected local open failure", str(target))

    def fail_local_open(path, *args, **kwargs):
        if path == str(target):
            raise failure
        return local_open(path, *args, **kwargs)

    monkeypatch.setattr(builtins, "open", fail_local_open)
    with pytest.raises(PermissionError) as error:
        dummy_fs.get("/source", str(target))
    assert error.value is failure
    assert target.read_bytes() == b"original local contents"
    assert not dummy_fs._client._fds
