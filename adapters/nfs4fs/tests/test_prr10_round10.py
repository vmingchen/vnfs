"""PRR round 10: final flush failures still release scalar writer handles."""

import errno

import pytest


@pytest.mark.parametrize("mode", ["wb", "ab"])
@pytest.mark.parametrize("connection_lost", [False, True])
@pytest.mark.parametrize("close_fails", [False, True])
def test_final_flush_failure_closes_or_defers_handle_without_replay(
    dummy_fs, monkeypatch, mode, connection_lost, close_fails
):
    dummy_fs.pipe_file("/data", b"seed")
    writer = dummy_fs.open("/data", mode, write_buffering=True, block_size=128)
    token = writer._fd
    assert writer.write(b"payload") == 7
    primary = (
        ConnectionError("injected lost write reply")
        if connection_lost
        else OSError(errno.ENOSPC, "injected full server")
    )
    cleanup = OSError(errno.EIO, "injected CLOSE failure")
    original_write = writer._raw.write
    original_close = dummy_fs._client.close
    writes = []
    closes = []

    def partial_then_fail(data):
        writes.append(bytes(data))
        if len(writes) == 1:
            return original_write(data[:2])
        raise primary

    def close(fd):
        closes.append(fd)
        if close_fails:
            raise cleanup
        return original_close(fd)

    try:
        with monkeypatch.context() as patch:
            patch.setattr(writer._raw, "write", partial_then_fail)
            patch.setattr(dummy_fs._client, "close", close)
            with pytest.raises(type(primary)) as error:
                writer.close()
            assert error.value is primary
            assert writes == [b"payload", b"yload"]
            assert closes == [token]
            assert writer.closed is (not close_fails)
            assert writer._raw.closed is (not close_fails)
            if close_fails:
                assert error.value.__cause__ is cleanup
                assert dummy_fs._client.descriptor_valid(token)
                assert token in dummy_fs._client._deferred_close
            else:
                assert not dummy_fs._client.descriptor_valid(token)
                assert not dummy_fs._client._deferred_close

        # Ordinary path operations must clean an outstanding CLOSE even when
        # callers stop retaining the failed writer. Neither cleanup nor an
        # explicit close retry may upload the committed prefix again.
        assert dummy_fs.info("/data")["size"] == (6 if mode == "ab" else 2)
        assert not dummy_fs._client.descriptor_valid(token)
        assert not dummy_fs._client._deferred_close
        if close_fails:
            with pytest.raises(type(primary), match="unusable"):
                writer.close()
        assert writer.closed
        writer.close()
        assert writes == [b"payload", b"yload"]
        assert dummy_fs.cat_file("/data") == (b"seedpa" if mode == "ab" else b"pa")
    finally:
        if not writer.closed:
            try:
                writer.close()
            except (OSError, ConnectionError):
                pass


@pytest.mark.parametrize("mode", ["wb", "ab"])
@pytest.mark.parametrize("reconnect_fails", [False, True])
@pytest.mark.parametrize("close_fails", [False, True])
def test_native_final_flush_connection_loss_retains_cleanup_ownership(
    dummy_fs, monkeypatch, mode, reconnect_fails, close_fails
):
    dummy_fs.auto_reconnect = reconnect_fails
    dummy_fs.pipe_file("/data", b"seed")
    writer = dummy_fs.open("/data", mode, write_buffering=True, block_size=128)
    token = writer._fd
    writer.write(b"payload")
    method = "write_positioned" if mode == "ab" else "pwrite"
    original_write = getattr(dummy_fs._client, method)
    original_close = dummy_fs._client.close
    primary = ConnectionError("injected lost write reply")
    cleanup = OSError(errno.EIO, "injected CLOSE failure")
    writes = []
    closes = []
    reconnects = []

    def lose_reply(fd, data, *args):
        writes.append(bytes(data))
        if len(writes) == 1:
            return original_write(fd, data[:2], *args)
        # The server accepts another byte before the reply is lost. Cleanup
        # must never replay either the known prefix or this ambiguous write.
        original_write(fd, data[:1], *args)
        raise primary

    def fail_reconnect(fd):
        reconnects.append(fd)
        raise ConnectionError("injected reconnect failure")

    def close(fd):
        closes.append(fd)
        if close_fails:
            raise cleanup
        return original_close(fd)

    try:
        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, method, lose_reply)
            patch.setattr(dummy_fs._client, "reconnect_descriptor", fail_reconnect)
            patch.setattr(dummy_fs._client, "close", close)
            with pytest.raises(ConnectionError) as error:
                writer.close()
            assert error.value is primary
            assert writes == [b"payload", b"yload"]
            assert reconnects == ([token] if reconnect_fails else [])
            assert closes == [token]
            assert writer.closed is (not close_fails)
            if close_fails:
                assert error.value.__cause__ is cleanup
                assert writer._fd == token
                assert dummy_fs._client.descriptor_valid(token)
                assert token in dummy_fs._client._deferred_close
            else:
                assert writer._fd is None
                assert not dummy_fs._client.descriptor_valid(token)

        assert dummy_fs.info("/data")["size"] == (7 if mode == "ab" else 3)
        assert not dummy_fs._client.descriptor_valid(token)
        assert not dummy_fs._client._deferred_close
        if close_fails:
            with pytest.raises(ConnectionError, match="unusable"):
                writer.close()
        assert writer.closed
        writer.close()
        assert writes == [b"payload", b"yload"]
        assert dummy_fs.cat_file("/data") == (b"seedpay" if mode == "ab" else b"pay")
    finally:
        if not writer.closed:
            try:
                writer.close()
            except (OSError, ConnectionError):
                pass
