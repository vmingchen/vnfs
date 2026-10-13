"""PRR round 1: shared streamed-read cleanup preserves primary failures."""

import errno

import pytest


@pytest.mark.parametrize("operation", ["cat", "append"])
@pytest.mark.parametrize("failure_at", ["read", "close"])
def test_shared_stream_cleanup_owns_failed_close(
    dummy_fs, monkeypatch, operation, failure_at
):
    dummy_fs.pipe_file("/source", b"original payload")
    dummy_fs.max_batch_bytes = 1
    primary = OSError(errno.ENOSPC, "injected read failure")
    cleanup = PermissionError(errno.EACCES, "injected close failure")
    attempted = []

    def fail_read(*args):
        raise primary

    def fail_close(fd):
        attempted.append(fd)
        raise cleanup

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "close", fail_close)
        if failure_at == "read":
            patch.setattr(dummy_fs._client, "pread", fail_read)
        expected = primary if failure_at == "read" else cleanup
        with pytest.raises(type(expected)) as error:
            if operation == "cat":
                dummy_fs.cat(["/source"])
            else:
                dummy_fs.open("/source", "ab", autocommit=False)
        assert error.value is expected
        assert (
            set(dummy_fs._client._fds)
            == dummy_fs._client._deferred_close
            == set(attempted)
        )
    assert dummy_fs.info("/source")["size"] == len(b"original payload")
    assert not dummy_fs._client._fds
    assert dummy_fs.cat_file("/source") == b"original payload"


def test_shared_stream_allocation_failure_survives_failed_close(dummy_fs, monkeypatch):
    dummy_fs.pipe_file("/source", b"abcd")
    dummy_fs.max_batch_bytes = 1
    dummy_fs.read_all_max_total_bytes = 4
    stat_many = dummy_fs._client.stat_many

    def grow_after_stat(paths):
        result = stat_many(paths)
        dummy_fs._client.write_many(
            [dummy_fs._native_path("/source")], [b"abcdefgh"], truncate=True
        )
        return result

    def fail_close(fd):
        raise PermissionError(errno.EACCES, "injected close failure")

    with monkeypatch.context() as patch:
        patch.setattr(dummy_fs._client, "stat_many", grow_after_stat)
        patch.setattr(dummy_fs._client, "close", fail_close)
        with pytest.raises(OSError) as error:
            dummy_fs.cat(["/source"])
        assert error.value.errno == errno.EFBIG
        assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
    dummy_fs.info("/source")
    assert not dummy_fs._client._fds


@pytest.mark.parametrize("operation", ["pipe", "put", "prepare"])
@pytest.mark.parametrize("failure_at", ["write", "close"])
def test_shared_streamed_writers_preserve_failure_and_own_cleanup(
    dummy_fs, tmp_path, monkeypatch, operation, failure_at
):
    payload = b"replacement payload"
    dummy_fs.max_batch_bytes = 1
    local = tmp_path / "source"
    local.write_bytes(payload)
    primary = OSError(errno.ENOSPC, "injected write failure")
    cleanup = PermissionError(errno.EACCES, "injected close failure")
    attempted = []
    file = None
    if operation == "prepare":
        file = dummy_fs.open("/destination", "wb", autocommit=False)
        file.write(payload)

    def fail_write(*args):
        raise primary

    def fail_close(fd):
        attempted.append(fd)
        raise cleanup

    try:
        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, "close", fail_close)
            if failure_at == "write":
                patch.setattr(dummy_fs._client, "pwrite", fail_write)
            expected = primary if failure_at == "write" else cleanup
            with pytest.raises(type(expected)) as error:
                if operation == "pipe":
                    dummy_fs.pipe_file("/destination", payload)
                elif operation == "put":
                    dummy_fs.put(str(local), "/destination")
                else:
                    file.prepare()
            assert error.value is expected
            assert len(set(attempted)) == 1
            # Preparing a transaction may retry CLOSE while unlinking its stage.
            # Any token not already closed must still have cleanup ownership.
            assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
            assert dummy_fs._client._deferred_close <= set(attempted)
        dummy_fs.info("/")
        assert not dummy_fs._client._fds
        if operation == "prepare":
            assert not dummy_fs.exists(file.temp_path)
            assert not dummy_fs.exists("/destination")
        elif failure_at == "close":
            assert dummy_fs.cat_file("/destination") == payload
    finally:
        if file is not None:
            file.discard()
