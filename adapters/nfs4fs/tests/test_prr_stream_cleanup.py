"""Stream exit preserves failures and owns handles whose CLOSE must retry."""

import errno

import fsspec
import pytest


@pytest.mark.parametrize(
    "failure_at", ["success", "callback", "cancel", "read", "metadata"]
)
@pytest.mark.parametrize("close_error_type", [OSError, KeyboardInterrupt])
def test_pipelined_failed_close_preserves_primary_and_defers_cleanup(
    tmp_path, monkeypatch, failure_at, close_error_type
):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path / "root"),
        connection_pool_size=2,
        skip_instance_cache=True,
    ) as fs:
        fs.pipe_file("/data", b"AAAABBBB")
        primary = (
            KeyboardInterrupt("injected cancellation")
            if failure_at == "cancel"
            else (
                RuntimeError("injected callback failure")
                if failure_at == "callback"
                else OSError(errno.EIO, "injected operation failure")
            )
        )
        cleanup = close_error_type("first injected CLOSE failure")
        later_cleanup = close_error_type("later injected CLOSE failure")
        attempted = []
        chunks = []

        def fail_close(fd):
            attempted.append(fd)
            raise cleanup if len(set(attempted)) == 1 else later_cleanup

        def fail_operation(*args, **kwargs):
            raise primary

        def receive(offset, data):
            chunks.append((offset, data))
            if failure_at in ("callback", "cancel"):
                raise primary

        with monkeypatch.context() as patch:
            patch.setattr(fs._client, "close", fail_close)
            if failure_at in ("read", "metadata"):
                patch.setattr(
                    fs._client,
                    "pread" if failure_at == "read" else "fstat",
                    fail_operation,
                )
            expected = cleanup if failure_at == "success" else primary
            with pytest.raises(type(expected)) as error:
                fs.read_stream_pipelined(
                    "/data", receive, workers=2, chunk_size=4, max_in_flight=1
                )
            assert error.value is expected
            assert len(set(attempted)) == 2
            assert fs._client._fds
            assert set(fs._client._fds) <= fs._client._deferred_close
        # A subsequent path call owns the retry even though stream readers died.
        assert fs.info("/data")["size"] == 8
        assert not fs._client._fds
        assert not fs._client._deferred_close
        assert fs.cat_file("/data") == b"AAAABBBB"
        if failure_at == "success":
            assert chunks == [(0, b"AAAA"), (4, b"BBBB")]
