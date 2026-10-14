"""PRR round 5: failed append construction immediately releases disk spools."""

import errno
import os
import weakref

import pytest
from vfsi_fsspec import _fs


@pytest.mark.parametrize("failure", ["read", "reader_close"])
@pytest.mark.parametrize("spool_close_fails", [False, True])
def test_failed_append_constructor_closes_rolled_spool(
    dummy_fs, monkeypatch, failure, spool_close_fails
):
    dummy_fs.transaction_spool_threshold = 1
    dummy_fs.transfer_chunk_size = 2
    dummy_fs.pipe_file("/source", b"abcdefgh")
    original_factory = _fs.tempfile.SpooledTemporaryFile
    original_read = dummy_fs._client.pread
    references = []
    descriptor = []
    reads = []
    primary = OSError(errno.EIO, "injected append setup failure")
    cleanup = OSError(errno.EACCES, "injected spool CLOSE failure")

    def spool_factory(*args, **kwargs):
        spool = original_factory(*args, **kwargs)
        references.append(weakref.ref(spool))
        if spool_close_fails:
            close = spool.close
            attempted = []

            def close_then_fail():
                close()
                if not attempted:
                    attempted.append(True)
                    raise cleanup

            spool.close = close_then_fail
        return spool

    def save_descriptor():
        spool = references[0]()
        assert spool._rolled
        descriptor.append(spool.fileno())

    def fail_read(fd, length, offset):
        reads.append(offset)
        if len(reads) == 2:
            save_descriptor()
            raise primary
        return original_read(fd, length, offset)

    def fail_reader_close(fd):
        save_descriptor()
        raise primary

    with monkeypatch.context() as patch:
        patch.setattr(_fs.tempfile, "SpooledTemporaryFile", spool_factory)
        patch.setattr(
            dummy_fs._client,
            "pread" if failure == "read" else "close",
            fail_read if failure == "read" else fail_reader_close,
        )
        with pytest.raises(OSError) as error:
            with dummy_fs.transaction as transaction:
                dummy_fs.open("/source", "ab")
        # Keep the error and its constructor traceback alive while checking
        # closure; neither garbage collection nor rollback owns this spool.
        assert error.value is primary
        spool = references[0]()
        assert spool is not None
        assert spool._rolled and spool.closed
        assert descriptor
        with pytest.raises(OSError) as closed_fd:
            os.fstat(descriptor[0])
        assert closed_fd.value.errno == errno.EBADF
        if spool_close_fails:
            assert error.value.__cause__ is cleanup
        assert not transaction.files
        assert not dummy_fs._intrans
        assert dummy_fs._transaction is None
        assert set(dummy_fs._client._fds) <= dummy_fs._client._deferred_close
    dummy_fs.info("/source")
    assert not dummy_fs._client._fds
    assert not dummy_fs._client._deferred_close
    assert dummy_fs.cat_file("/source") == b"abcdefgh"
    assert dummy_fs.find("/") == ["/source"]


def test_successful_append_constructor_transfers_spool_ownership(dummy_fs, monkeypatch):
    dummy_fs.transaction_spool_threshold = 1
    dummy_fs.transfer_chunk_size = 2
    dummy_fs.pipe_file("/source", b"abcdefgh")
    with dummy_fs.transaction:
        file = dummy_fs.open("/source", "ab")
        spool = file._spool
        assert spool._rolled and not spool.closed
        file.write(b"tail")
        file.close()
        assert not spool.closed
        assert dummy_fs.cat_file("/source") == b"abcdefgh"
    assert spool.closed
    assert dummy_fs.cat_file("/source") == b"abcdefghtail"
    assert not dummy_fs._client._fds
