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
