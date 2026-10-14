"""PRR round 13: ranged transport errors retain authorized recovery policy."""

import pytest


@pytest.mark.parametrize("operation", ["cat_file", "cat_ranges", "cat"])
@pytest.mark.parametrize("reconnect", [False, True])
@pytest.mark.parametrize("indexed", [False, True])
def test_ranged_transport_retains_connection_error_policy(
    dummy_fs, monkeypatch, operation, reconnect, indexed
):
    dummy_fs.pipe({"/a": b"alpha", "/b": b"beta"})
    session = dummy_fs._client._clients[0]
    monkeypatch.setattr(dummy_fs, "auto_reconnect", reconnect)
    monkeypatch.setattr(dummy_fs._client, "_auto_reconnect", reconnect)
    original = session._native
    generation = session.generation
    primary = ConnectionError("injected native ranged transport")
    if indexed:
        primary.index = 0
    calls, reconnects = [], []

    class FaultNative:
        def read_many(self, *args):
            calls.append(args)
            raise primary

        def __getattr__(self, name):
            return getattr(original, name)

    session._native = FaultNative()
    reconnect_session = session.reconnect

    def record_reconnect(*args, **kwargs):
        reconnects.append(True)
        return reconnect_session(*args, **kwargs)

    monkeypatch.setattr(session, "reconnect", record_reconnect)

    def read():
        if operation == "cat_file":
            return dummy_fs.cat_file("/a", start=0, end=2)
        if operation == "cat_ranges":
            return dummy_fs.cat_ranges(["/a", "/b"], [0, 0], [2, 2], on_error="return")
        return dummy_fs.cat(["/a", "/b"], start=0, end=2, on_error="return")

    if reconnect:
        result = read()
        expected = (
            b"al"
            if operation == "cat_file"
            else (
                [b"al", b"be"]
                if operation == "cat_ranges"
                else {"/a": b"al", "/b": b"be"}
            )
        )
        assert result == expected
        assert len(reconnects) == 1
        assert session.generation == generation + 1
    else:
        with pytest.raises(ConnectionError) as error:
            read()
        assert error.value is primary
        assert not reconnects
        assert session.generation == generation
    assert len(calls) == 1
    assert not dummy_fs._client._fds
