"""PRR round 3: transactional wrapper setup retains primary errors and tokens."""

import errno

import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize("failure", ["encoding", "missing"])
@pytest.mark.parametrize("close_fails", [False, True])
@pytest.mark.parametrize("reader_first", [False, True])
def test_transactional_wrapper_failure_preserves_setup_error_and_cleanup(
    dummy_fs, monkeypatch, failure, close_fails, reader_first
):
    dummy_fs.pipe_file("/input", b"input data")
    dummy_fs.pipe_file("/bad", b"bad data")
    bad = (
        OpenFile(dummy_fs, "/bad", "rt", encoding="prr-invalid-codec")
        if failure == "encoding"
        else OpenFile(dummy_fs, "/missing", "rb")
    )
    reader = OpenFile(dummy_fs, "/input", "rb")
    writer = OpenFile(dummy_fs, "/output", "wb")
    entries = [bad, reader, writer] if reader_first else [reader, writer, bad]
    cleanup = PermissionError(errno.EACCES, "injected CLOSE failure")
    transaction = dummy_fs.start_transaction()
    original_close = dummy_fs._client.close
    attempted = []
    expected_tokens = int(not reader_first) + int(failure == "encoding")

    def close(fd):
        attempted.append(fd)
        if close_fails:
            raise cleanup
        return original_close(fd)

    try:
        with monkeypatch.context() as patch:
            patch.setattr(dummy_fs._client, "close", close)
            expected = LookupError if failure == "encoding" else FileNotFoundError
            with pytest.raises(expected) as error:
                dummy_fs.open_many(entries)
            if failure == "encoding":
                assert "prr-invalid-codec" in str(error.value)
            else:
                assert "/missing" in str(error.value)
            assert len(set(attempted)) == expected_tokens
            if close_fails and expected_tokens:
                assert error.value.__cause__ is cleanup
                assert set(dummy_fs._client._fds) == set(attempted)
                assert set(dummy_fs._client._fds) == dummy_fs._client._deferred_close
            else:
                assert not dummy_fs._client._fds
        assert dummy_fs.info("/input")["size"] == 10
        assert not dummy_fs._client._fds
        assert not dummy_fs._client._deferred_close
        for entry in entries:
            entry.close()
        assert not dummy_fs.exists("/output")
    finally:
        transaction.complete(commit=False)
    assert not dummy_fs._intrans
    assert dummy_fs.cat_file("/input") == b"input data"
