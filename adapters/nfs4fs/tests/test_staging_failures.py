"""Local staging failures must never replay bytes or hide primary errors."""

import errno
import os

import pytest
from fsspec.core import OpenFile


def _writers(fs, count=2, mode="wb"):
    fs.write_buffering = True
    fs.block_size = 16
    return fs.open_many([OpenFile(fs, f"/file-{i}", mode=mode) for i in range(count)])


@pytest.mark.parametrize("mode", ["wb", "ab"])
@pytest.mark.parametrize("timing", ["flush", "close"])
def test_partial_staging_failure_poisoned_writer_cannot_replay(
    dummy_fs, monkeypatch, mode, timing
):
    fs = dummy_fs
    fs.pipe({"/file-0": b"old", "/file-1": b"old"})
    files = _writers(fs, mode=mode)
    failed, healthy = files
    spool = failed._write_spool
    original = spool.write
    failure = OSError(errno.ENOSPC, "local spool full")
    stages = []

    def partial(data):
        stages.append(bytes(data))
        original(data[:2])
        raise failure

    try:
        failed.write(b"abcd")
        monkeypatch.setattr(spool, "write", partial)
        with pytest.raises(OSError) as raised:
            getattr(failed, timing)()
        assert raised.value is failure
        monkeypatch.setattr(spool, "write", original)
        assert failed._write_failed
        assert failed._buffer_group.in_memory_bytes() == 0
        if timing == "flush":
            for operation in (
                lambda: failed.write(b"again"),
                failed.flush,
                failed.close,
            ):
                with pytest.raises(OSError) as poisoned:
                    operation()
                assert poisoned.value.errno == errno.ENOSPC
        assert failed.closed
        healthy.write(b"healthy")
        healthy.close()
        assert stages == [b"abcd"]
        assert fs.cat_file("/file-0") == (b"old" if mode == "ab" else b"")
        assert fs.cat_file("/file-1") == (b"old" if mode == "ab" else b"") + b"healthy"
        assert not fs._client._fds
        assert not fs._client._deferred_close
    finally:
        monkeypatch.setattr(spool, "write", original)
        try:
            fs.commit_many(files)
        except OSError:
            pass


@pytest.mark.parametrize("mode", ["wb", "ab"])
def test_short_local_stage_writes_are_completed(dummy_fs, monkeypatch, mode):
    fs = dummy_fs
    fs.pipe({"/file-0": b"old", "/file-1": b"old"})
    files = _writers(fs, mode=mode)
    spool = files[0]._write_spool
    original = spool.write
    calls = []

    def short(data):
        calls.append(bytes(data))
        return original(data[:2] if len(calls) == 1 else data)

    monkeypatch.setattr(spool, "write", short)
    try:
        files[0].write(b"abcd")
        files[0].flush()
        files[0].close()
        files[1].close()
        assert fs.cat_file("/file-0") == (b"old" if mode == "ab" else b"") + b"abcd"
        assert calls == [b"abcd", b"cd"]
        assert not files[0]._write_failed
        assert not fs._client._fds
    finally:
        fs.commit_many(files)


def test_failed_rollover_poisons_its_owner_and_staging_caller(dummy_fs, monkeypatch):
    fs = dummy_fs
    fs.write_buffering = True
    fs.block_size = 4
    fs.max_batch_bytes = 8
    files = fs.open_many([OpenFile(fs, f"/file-{i}", mode="wb") for i in range(3)])
    failed_owner, healthy, caller = files
    # Leave each earlier writer with a full in-memory spool and a partial
    # buffer. The third writer then crosses the shared limit.
    failed_owner.write(b"aa")
    failed_owner.write(b"a")
    healthy.write(b"bb")
    healthy.write(b"b")
    spool = failed_owner._write_spool
    assert not spool._rolled
    failure = OSError(errno.ENOSPC, "rollover full")
    rollovers = []

    def partial_rollover():
        rollovers.append(True)
        spool.truncate(1)
        raise failure

    monkeypatch.setattr(spool, "rollover", partial_rollover)
    try:
        with pytest.raises(OSError) as raised:
            caller.write(b"cc")
        assert raised.value is failure
        assert failed_owner._write_failed
        assert caller._write_failed
        assert not healthy._write_failed
        assert healthy._buffer_group.in_memory_bytes() == 3
        for file in (failed_owner, caller):
            with pytest.raises(OSError) as raised:
                file.close()
            assert raised.value.errno == errno.ENOSPC
        healthy.close()
        assert rollovers == [True]
        assert fs.cat_file("/file-0") == b""
        assert fs.cat_file("/file-1") == b"bbb"
        assert fs.cat_file("/file-2") == b""
        assert not fs._client._fds
    finally:
        try:
            fs.commit_many(files)
        except OSError:
            pass


@pytest.mark.parametrize("close_first", [False, True])
def test_staging_failure_keeps_cleanup_error_and_retry_ownership(
    dummy_fs, monkeypatch, close_first
):
    fs = dummy_fs
    files = _writers(fs)
    failed, healthy = files
    spool = failed._write_spool
    original_write, original_close = spool.write, spool.close
    primary = OSError(errno.ENOSPC, "stage full")
    cleanup = OSError(errno.EIO, "spool cleanup failed")

    def write(data):
        original_write(data[:2])
        raise primary

    def close():
        if close_first:
            original_close()
        raise cleanup

    try:
        failed.write(b"abcd")
        monkeypatch.setattr(spool, "write", write)
        monkeypatch.setattr(spool, "close", close)
        with pytest.raises(OSError) as raised:
            failed.flush()
        assert raised.value is primary
        assert primary.__cause__ is cleanup
        assert failed._write_failed
        monkeypatch.setattr(spool, "write", original_write)
        monkeypatch.setattr(spool, "close", original_close)
        with pytest.raises(OSError) as raised:
            failed.close()
        assert raised.value.errno == errno.ENOSPC
        healthy.write(b"healthy")
        healthy.close()
        assert spool.closed
        assert fs.cat_file("/file-0") == b""
        assert fs.cat_file("/file-1") == b"healthy"
        assert not fs._client._fds
    finally:
        monkeypatch.setattr(spool, "write", original_write)
        monkeypatch.setattr(spool, "close", original_close)
        try:
            fs.commit_many(files)
        except OSError:
            pass


@pytest.mark.parametrize("body_type", [ValueError, KeyboardInterrupt])
@pytest.mark.parametrize("cleanup_phase", ["remote", "spool"])
def test_transaction_abort_preserves_body_error(
    dummy_fs, tmp_path, monkeypatch, body_type, cleanup_phase
):
    fs = dummy_fs
    fs.mkdir("/stages")
    primary = body_type("application failed")
    files = []
    original_close = None
    cleanup = OSError(errno.ENOSPC, "spool close failed")
    try:
        with pytest.raises(body_type) as raised:
            with fs.transaction:
                for path in ["/stages/file", "/sibling"]:
                    with fs.open(path, "wb") as file:
                        file.write(b"data")
                    files.append(file)
                if cleanup_phase == "remote":
                    files[0].prepare()
                    os.chmod(tmp_path / "root" / "stages", 0o555)
                else:
                    original_close = files[0]._spool.close

                    def fail_close():
                        original_close()
                        raise cleanup

                    monkeypatch.setattr(files[0]._spool, "close", fail_close)
                raise primary
        assert raised.value is primary
        if cleanup_phase == "remote":
            assert isinstance(primary.__cause__, PermissionError)
            assert primary.__cause__.errno == errno.EACCES
        else:
            assert primary.__cause__ is cleanup
        assert all(file._spool.closed for file in files)
        assert not fs._intrans
        assert fs._transaction is None
        assert not fs._client._fds
    finally:
        os.chmod(tmp_path / "root" / "stages", 0o755)
        if original_close is not None:
            monkeypatch.setattr(files[0]._spool, "close", original_close)
        for file in files:
            file.discard()
    assert not fs.exists("/stages/file")
    assert not fs.exists("/sibling")
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/stages", detail=False))


def test_transaction_cleanup_failure_still_surfaces_without_body_error(
    dummy_fs, monkeypatch
):
    fs = dummy_fs
    cleanup = OSError(errno.EIO, "close failed")
    closes = []
    with pytest.raises(OSError) as raised:
        with fs.transaction:
            with fs.open("/committed", "wb") as file:
                file.write(b"data")
            original = file._spool.close

            def fail_once():
                original()
                closes.append(True)
                if len(closes) == 1:
                    raise cleanup

            monkeypatch.setattr(file._spool, "close", fail_once)
    assert raised.value is cleanup
    assert fs.cat_file("/committed") == b"data"
    assert not fs._client._fds
    assert not fs._intrans
    assert fs._transaction is None


@pytest.mark.parametrize("operation", ["write", "truncate", "flush"])
def test_caught_spool_failure_aborts_transaction(dummy_fs, monkeypatch, operation):
    fs = dummy_fs
    previous = {"/existing": b"original", "/sibling": b"healthy"}
    fs.pipe(previous)
    failure = OSError(errno.ENOSPC, "local spool full")

    with pytest.raises(OSError) as aborted:
        with fs.transaction:
            with fs.open("/sibling", "wb") as sibling:
                sibling.write(b"new sibling")

            with fs.open("/existing", "wb") as file:
                spool = file._spool
                write_prefix = spool.write
                truncate = spool.truncate
                args = ()
                if operation == "write":
                    args = (b"replacement",)
                else:
                    file.write(b"replacement")
                    if operation == "truncate":
                        args = (2,)

                def fail(*args):
                    if operation == "write":
                        write_prefix(args[0][:2])
                    elif operation == "truncate":
                        truncate(*args)
                    raise failure

                with monkeypatch.context() as patch:
                    patch.setattr(spool, operation, fail)
                    with pytest.raises(OSError) as caught:
                        getattr(file, operation)(*args)
                    assert caught.value is failure

    assert aborted.value is failure
    assert fs.cat(list(previous)) == previous
    assert len(fs.ls("/", detail=False)) == 2
    assert file._spool.closed
    assert sibling._spool.closed


@pytest.mark.parametrize("operation", ["seek", "truncate"])
def test_caught_invalid_spool_request_keeps_transaction_usable(dummy_fs, operation):
    fs = dummy_fs
    fs.transaction_spool_threshold = 1
    with fs.transaction:
        with fs.open("/out", "wb") as file:
            file.write(b"hello")
            assert file._spool._rolled
            with pytest.raises(OSError) as caught:
                getattr(file, operation)(-1)
            assert caught.value.errno == errno.EINVAL
            file.write(b"!")
    assert fs.cat_file("/out") == b"hello!"
